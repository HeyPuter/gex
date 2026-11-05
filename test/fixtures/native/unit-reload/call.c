/* libcall.so, a function-tier unit: its call through a pointer leaves the
   unit, so it goes through the probe cache and the function table. */
int call_f(int (*fp)(int), int x) { return fp(x) + 0; }
