/* Region entries (test/wow64/regions): reg_body's code from reg_mid on,
 * an address inside the function that no function starts at, is called
 * from the program's own code through a pointer it computes at run time
 * (no relocation names it, so the unit's recovery cannot see it as an
 * entry). With a region entry built for it, the call is served there;
 * without one it leaves the unit. Either way the results are the code's. */
#include <stdio.h>
#include <stdlib.h>

int reg_body(int x);
extern const int reg_mid_off;
/* reg_body(x) = 100 + x; entered at reg_mid with eax = a, the result is a + x. */
__asm__(".text\n"
        ".globl _reg_body\n"
        "_reg_body:\n"
        "    movl $100, %eax\n"
        ".globl _reg_mid\n"
        "_reg_mid:\n"
        "    addl 4(%esp), %eax\n"
        "    ret\n"
        ".section .rdata,\"dr\"\n"
        ".globl _reg_mid_off\n"
        "_reg_mid_off:\n"
        "    .long _reg_mid - _reg_body\n"
        ".text\n");

static __attribute__((noinline)) int call_mid(const void *p, int a, int x)
{
    int r;
    __asm__ volatile("pushl %3\n\t"
                     "call *%1\n\t"
                     "addl $4, %%esp"
                     : "=a"(r)
                     : "r"(p), "0"(a), "r"(x)
                     : "ecx", "edx", "memory", "cc");
    return r;
}

int main(int argc, char **argv)
{
    volatile int off = reg_mid_off;
    const char *mid = (const char *)reg_body + off;
    int n = argc > 1 ? atoi(argv[1]) : 5000, bad = 0;
    for (int i = 0; i < n; i++) {
        if (reg_body(i) != 100 + i) bad++;
        if (call_mid(mid, 1000, i) != 1000 + i) bad++;
    }
    printf("regions %d calls, %d wrong\n", n, bad);
    printf("regions done\n");
    return bad != 0;
}
