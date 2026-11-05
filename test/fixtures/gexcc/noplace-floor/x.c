/* test/gexcc/noplace-floor's translated program: where its first mmap lands. */
#include <stdio.h>
#include <sys/mman.h>
int main(int argc, char **argv) {
    void *p = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    printf("x86 %s mmap %lu\n", argc > 1 ? argv[1] : "?", (unsigned long)p >> 20);
    return 0;
}
