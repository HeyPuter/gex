/* liba.so (-DK=3 -DC=1, a function-tier unit) and libb.so (-DK=7 -DC=-2,
   none): the same code at the same offsets (only the data differs), loaded
   in turn at one base. */
static volatile int k = K, c = C;
__attribute__((noinline)) static int inner(int x) { return x * k + c; }
int f(int x) { int s = 0; for (int i = 0; i < x; i++) s += inner(i); return s + inner(x); }
