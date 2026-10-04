/* two.dll: calls one.dll's export directly (one module, one call). */
int one_add(int a, int b);
static int helper(int x) { return x * 2; }
int shared_name(int x) { return helper(x) + 1; }
int two_call(int a, int b) { return one_add(a, b) * 10 + shared_name(1); }
