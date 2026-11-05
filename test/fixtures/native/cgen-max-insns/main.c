/* test/native/cgen-max-insns's program: big(), small() and flatcall() from libbig.so. */
#include <stdio.h>
long big(void *p, long *a);
long small(long x);
long flatcall(long x);
int main(void) {
    long a[8] = {1, 2, 3, 4, 5, 6, 7, 8}, s = 0;
    for (int i = 0; i < 50; i++) {
        a[i % 8] += big(a, a);
        s += small(a[i % 8]) + flatcall(s);
    }
    printf("big %ld small %ld\n", a[0] ^ a[3] ^ a[7], s);
    return 0;
}
