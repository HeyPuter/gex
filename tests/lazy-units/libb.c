/* Each on a page of its own: main.c rewrites b_two's. */
__attribute__((noinline, aligned(4096))) int b_mul(int x) { return x * 7; }
__attribute__((noinline, aligned(4096))) int b_two(int x) { return x + 7; }
