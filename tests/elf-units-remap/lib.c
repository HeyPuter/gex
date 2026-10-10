/* liba.so (-DK=3 -DC=1) and libb.so (-DK=7 -DC=-2): the same layout, different code. */
__attribute__((noinline)) static int inner(int x) { return x * K + C; }
int f(int x) { int s = 0; for (int i = 0; i < x; i++) s += inner(i); return s + inner(x); }
