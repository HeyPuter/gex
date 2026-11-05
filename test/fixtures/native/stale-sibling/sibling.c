// A block that starts on one code page and runs onto the next, reached by a
// direct branch from a block of its own translation that lies wholly on the
// first page. The second page's translations are then flushed (an icache
// invalidation of it, the code unchanged), so the spanning block fails its
// page check while the one that branches to it does not.
#include <stdio.h>

long step(long x, long y);
__asm__(
    ".text\n"
    ".balign 4096\n"
    ".space 4096 - 16\n"
    ".global step\n"
    "step:\n"
    "    cbz x1, 1f\n"          // P - 16: the sibling, on the first page
    "    add x0, x0, #2\n"
    "    ret\n"
    "1:  add x0, x0, #1\n"      // P - 4: starts on the first page...
    "    add x0, x0, #1\n"      // ...and runs onto the second
    "    add x0, x0, #1\n"
    "    ret\n"
    ".global second_page\n"
    "second_page = 1b + 4\n");
extern char second_page[];

int main(void) {
    long sum = 0;
    for (int i = 0; i < 20000; i++) {
        sum = step(sum, i & 1);
        if (i == 10)
            __builtin___clear_cache(second_page, second_page + 4);
    }
    printf("sum %ld\n", sum);
    return 0;
}
