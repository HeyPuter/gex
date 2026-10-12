/* vtext32.exe: a vtable kept in .text, as Microsoft's linker merges .rdata
 * into .text (Win7's uiribbon.dll), and a virtual call's tail
 * (`mov ecx, obj; mov eax, [obj]; jmp [eax + 4]`) through an object in
 * .data whose initial vtable pointer is relocated. Recovery resolves the
 * tail from the initial bytes as a one-case "switch" to `get_b`, which
 * only the table names otherwise (tests/recover_i386.rs).
 * Built by build.sh with i686-w64-mingw32-gcc -O2 -nostdlib. */
__declspec(dllimport) void __stdcall ExitProcess(unsigned);

int get_a(void);
int tail(void);
__asm__(".data\n"
        ".globl _obj\n"
        "_obj: .long _vt\n"
        "  .long 0x11111111\n"
        ".text\n"
        ".p2align 4\n"
        "_vt:\n"
        "  .long _get_a\n"
        "  .long _get_b\n"
        "  .byte 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90\n"
        "_get_b:\n"
        "  movl $0x2a, %eax\n"
        "  ret\n"
        "  .p2align 4\n"
        ".globl _tail\n"
        "_tail:\n"
        "  movl $_obj, %ecx\n"
        "  movl _obj, %eax\n"
        "  jmp *4(%eax)\n");

__attribute__((noinline)) int get_a(void) { return 7; }

void entry(void) {
    ExitProcess(tail() + get_a());
}
