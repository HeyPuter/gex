/* A unit's call to b_two, through its PLT: the probe cache's entry for
   b_two, which GEX_TEST_KEEP_PROBE keeps once b_two is retired. */
int b_two(int);
__attribute__((noinline)) int e_two(void) { return b_two(1000); }
