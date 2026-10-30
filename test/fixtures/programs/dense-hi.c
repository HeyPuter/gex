/* dense-exec: the guest of test/page/dense-exec.test.js. */
#include <stdio.h>
int main(int argc, char **argv) { printf("hi %s\n", argc > 1 ? argv[1] : "-"); return 0; }
