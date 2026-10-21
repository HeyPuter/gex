/* No Windows CRT startup: exercise imports, indirect calls, heap and relocation. */
#include <windows.h>

__attribute__((noinline)) static int calculate(int n) {
    int sum = 0;
    for (int i = 0; i < n; ++i) {
        sum += i * 3;
    }
    return sum;
}
static int (*volatile callback)(int) = calculate;

int entry(void) {
    DWORD written;
    HANDLE heap = GetProcessHeap();
    char *p = HeapAlloc(heap, HEAP_ZERO_MEMORY, 32);
    if (!p || p[0] || callback(7) != 63) {
        return 11;
    }
    const char message[] = "Hello from x64 PE!\n";
    for (unsigned i = 0; i < sizeof(message); ++i) {
        p[i] = message[i];
    }
    int ok = WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), p, sizeof(message) - 1, &written, 0);
    if (!HeapFree(heap, 0, p)) {
        return 12;
    }
    return ok && written == sizeof(message) - 1 ? 0 : 13;
}
