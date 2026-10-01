/* switch32.exe: a PE32 for function recovery's tests (tests/recover_i386.rs).
 * Built by build.sh with i686-w64-mingw32-gcc -O2 -nostdlib: no CRT, so
 * every function is one of these. */
__declspec(dllimport) void __stdcall ExitProcess(unsigned);

__attribute__((noinline)) int __stdcall scale(int x, int k) { return x * k + 1; }

__attribute__((noinline)) int pick(unsigned op, int x) {
    switch (op) {
    case 0: return x + 3;
    case 1: return x - 7;
    case 2: return x * 5;
    case 3: return x ^ 0x55;
    case 4: return scale(x, 9);
    case 5: return x >> 2;
    case 6: return -x;
    default: return 0;
    }
}

int (*volatile hook)(unsigned, int) = pick;

void entry(void) {
    int acc = 0;
    for (unsigned i = 0; i < 8; i++)
        acc += hook(i, acc + 1);
    ExitProcess(acc & 0xff);
}
