/* vtail64.exe: a PE32+ vtable in .rdata (relocated pointers) whose second
 * entry, `get_b`, is also reached by a direct tail jump from `pick`, laid
 * out right before it. Discovering `pick` takes `get_b` into its blocks;
 * only the table names `get_b` otherwise, so recovery keeps it an interior
 * entry of `pick` (tests/recover_pe64.rs). `get_c` is the same without the
 * tail jump, a function of its own. No unwind info, as for a leaf function.
 * Built by build.sh with x86_64-w64-mingw32-gcc -O2 -nostdlib. */
__declspec(dllimport) void ExitProcess(unsigned);

typedef int (*fn_t)(int);
extern const fn_t vt[3];
int pick(int);
__asm__(".section .rdata,\"dr\"\n"
        ".p2align 3\n"
        ".globl vt\n"
        "vt:\n"
        "  .quad get_a\n"
        "  .quad get_b\n"
        "  .quad get_c\n"
        ".text\n"
        ".p2align 4\n"
        ".globl pick\n"
        "pick:\n"
        "  testl %ecx, %ecx\n"
        "  jns get_b\n"
        "  movl $-1, %eax\n"
        "  ret\n"
        "  .p2align 4\n"
        "get_b:\n"
        "  leal 42(%rcx), %eax\n"
        "  ret\n"
        "  .p2align 4\n"
        "get_c:\n"
        "  leal 7(%rcx), %eax\n"
        "  ret\n");

__attribute__((noinline)) int get_a(int x) { return x * 3; }

void entry(void) {
    volatile int i = 1;
    ExitProcess(vt[i](1) + vt[i + 1](2) + pick(i) + get_a(i));
}
