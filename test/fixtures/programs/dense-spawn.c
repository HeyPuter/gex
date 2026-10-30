/* dense-exec: the guest of test/page/dense-exec.test.js. */
#include <stdio.h>
#include <string.h>
#include <unistd.h>
__attribute__((import_module("gex"), import_name("exec_fresh")))
extern void gex_exec_fresh(void);
/* ds fresh|plain PROG ARGS...: exec PROG, after exec_fresh for "fresh" */
int main(int argc, char **argv) {
    if (argc < 3) return 2;
    if (!strcmp(argv[1], "fresh")) gex_exec_fresh();
    execv(argv[2], argv + 2);
    perror("execv");
    return 127;
}
