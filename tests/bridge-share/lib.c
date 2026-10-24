/* Two entries of one unit, on pages of their own (K and C differ per library). */
__attribute__((noinline, aligned(4096))) int one(int x) { return x * K + C; }
__attribute__((noinline, aligned(4096))) int two(int x) { return (x ^ K) - C; }
