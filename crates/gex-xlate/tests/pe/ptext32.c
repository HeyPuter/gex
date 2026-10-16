/* ptext32.exe: a function pointer kept in .text, its target right after it,
 * as Win7's shell32.dll lays out its lazily bound wrappers (`[slot][nops]
 * [stub]`, the stub `mov edi,edi; push ebp; mov ebp,esp; ...; pop ebp;
 * jmp eax`), and callers that call through it (`call *_slot`). The call's
 * operand is a relocated address too, but of data: recovery must not take
 * the slot for a function, which swallowed the stub as an interior block
 * (tests/recover_i386.rs). `_three` tail-jumps through the slot, which
 * recovery reads as a one-case switch to the stub: a callee all the same. Built by build.sh with i686-w64-mingw32-gcc
 * -O2 -nostdlib. */
__declspec(dllimport) void __stdcall ExitProcess(unsigned);

int real(void);
int one(void);
int two(void);
int three(void);
__asm__(".data\n"
        "_bound: .long _real\n"
        ".text\n"
        ".p2align 4\n"
        "_slot:\n"
        "  .long _stub\n"
        "  .byte 0x90, 0x90, 0x90, 0x90, 0x90\n"
        "_stub:\n"
        "  movl %edi, %edi\n"
        "  pushl %ebp\n"
        "  movl %esp, %ebp\n"
        "  movl _bound, %eax\n"
        "  popl %ebp\n"
        "  jmp *%eax\n"
        "  .p2align 4\n"
        ".globl _one\n"
        "_one:\n"
        "  call *_slot\n"
        "  addl $1, %eax\n"
        "  ret\n"
        "  .p2align 4\n"
        ".globl _two\n"
        "_two:\n"
        "  call *_slot\n"
        "  addl $2, %eax\n"
        "  ret\n"
        "  .p2align 4\n"
        ".globl _three\n"
        "_three:\n"
        "  jmp *_slot\n");

__attribute__((noinline)) int real(void) { return 7; }

void entry(void) {
    ExitProcess(one() + two() + three());
}
