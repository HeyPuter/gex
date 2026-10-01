/* What the x87 leaves in the condition codes the manual calls undefined:
 * fprem and fprem1 with a NaN result (an invalid operand, or a NaN one)
 * clear C2 and C1 and keep C0 and C3 from before; with a number they report
 * the quotient bits. Each case starts from condition codes set by a compare
 * (less, unordered) or by fxam (C1). */
#include <math.h>
#include <stdint.h>
#include <stdio.h>

int main(void) {
    double cases[][2] = {{INFINITY, 1}, {1, 0}, {NAN, 1}, {1, NAN}, {0, 0}, {0, 5}, {5, INFINITY}, {7, 2}, {-7.5, 2}};
    double z = 0, one = 1, nan = NAN, m1 = -1;
    for (unsigned i = 0; i < sizeof cases / sizeof *cases; i++) {
        double a = cases[i][0], b = cases[i][1];
        uint16_t sw[6];
        __asm__ volatile("fninit\n fldl %3\n fldl %4\n fcompp\n fldl %2\n fldl %1\n fprem\n fnstsw %0\n fninit"
                         : "=m"(sw[0]) : "m"(a), "m"(b), "m"(one), "m"(z));
        __asm__ volatile("fninit\n fldl %3\n fldl %3\n fucompp\n fldl %2\n fldl %1\n fprem\n fnstsw %0\n fninit"
                         : "=m"(sw[1]) : "m"(a), "m"(b), "m"(nan));
        __asm__ volatile("fninit\n fldl %3\n fxam\n fstp %%st(0)\n fldl %2\n fldl %1\n fprem\n fnstsw %0\n fninit"
                         : "=m"(sw[2]) : "m"(a), "m"(b), "m"(m1));
        __asm__ volatile("fninit\n fldl %3\n fldl %4\n fcompp\n fldl %2\n fldl %1\n fprem1\n fnstsw %0\n fninit"
                         : "=m"(sw[3]) : "m"(a), "m"(b), "m"(one), "m"(z));
        __asm__ volatile("fninit\n fldl %3\n fldl %3\n fucompp\n fldl %2\n fldl %1\n fprem1\n fnstsw %0\n fninit"
                         : "=m"(sw[4]) : "m"(a), "m"(b), "m"(nan));
        __asm__ volatile("fninit\n fldl %3\n fxam\n fstp %%st(0)\n fldl %2\n fldl %1\n fprem1\n fnstsw %0\n fninit"
                         : "=m"(sw[5]) : "m"(a), "m"(b), "m"(m1));
        printf("%g rem %g:", a, b);
        for (int k = 0; k < 6; k++) printf(" %04x", sw[k] & 0x47ff);
        printf("\n");
    }
    return 0;
}
