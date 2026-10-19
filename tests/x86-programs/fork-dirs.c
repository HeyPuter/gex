/* A forked child runs code its parent translated, on several pages.
 *
 * The child's page table keeps its parent's page numbering, and its dispatch
 * directory is rebuilt without the parent's runtime entries. Its first
 * translation, on fork's return path, runs on into the next pages, which are
 * numbered but have no entry. Those blocks failed their page check, and
 * `heal_dirs` wrote the entries and said "gex: heal" (a bug line). run.sh fails
 * this test, in the page, on any heal line. */
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGE_FN(n) \
    __attribute__((noinline, aligned(4096))) static int f##n(int x) { return x * (n + 3) + n; }
PAGE_FN(0)
PAGE_FN(1)
PAGE_FN(2)
PAGE_FN(3)

static int all(int x) { return f0(x) + f1(x) + f2(x) + f3(x); }

int main(void) {
    int s = all(1);
    pid_t p = fork();
    if (p == 0) _exit(all(2) & 0x7f);
    int st = 0;
    waitpid(p, &st, 0);
    printf("parent %d child %d\n", s, WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    return 0;
}
