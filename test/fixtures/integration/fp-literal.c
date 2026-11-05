/* `ldr s0, <literal>` and `ldr d0, <literal>`: a float constant fetched from
   the pool beside the code. The decoder said "deferred to the NEON
   milestone" and returned unknown, so any function with a float constant it
   could not materialise ended in SIGILL. libXxf86vm has six. */
#include <stdio.h>
volatile double dv = 1.0;
volatile float fv = 1.0f;
int main(void) {
    double d = 0; float f = 0;
    __asm__ volatile("ldr d0, 1f\n\t" "fmov %d[o], d0\n\t" "b 2f\n\t"
                     ".align 3\n1:\t.double 3.14159265358979\n2:\n"
                     : [o] "=w"(d) :: "d0");
    __asm__ volatile("ldr s1, 3f\n\t" "fmov %s[o], s1\n\t" "b 4f\n\t"
                     ".align 2\n3:\t.float 2.71828\n4:\n"
                     : [o] "=w"(f) :: "s1");
    printf("%.14f %.5f\n", d, (double)f);
    /* And the ordinary compiler-generated case. */
    printf("%.4f %.4f\n", dv * 10000.0, (double)(fv * 100.0f));
    return 0;
}
