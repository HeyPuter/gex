/* xthr.c's DLL: a function another thread calls through a pointer before it
 * has made a system call since this DLL was loaded. */
static int add1(int x) { return x + 1; }
int (*xthr_get(void))(int) { return add1; }
int xthr_twice(int (*f)(int), int x) { return f(f(x)); }
