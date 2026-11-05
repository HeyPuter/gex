/* Its constructor calls into libb.so: in a Worker that has instantiated
   neither unit, the first instantiates the second from inside its call. */
int b_mul(int);
static int got;
__attribute__((constructor)) static void init(void) { got = b_mul(5); }
__attribute__((noinline, aligned(4096))) int d_get(void) { return got + 64; }
