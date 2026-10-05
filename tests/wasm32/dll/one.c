/* one.dll: exports abi_add too (as abi.dll does), and has a private
 * shared_name, as two.dll has an exported one: in the one-module PE side
 * neither may collide. */
int shared_name(int x) { return x - 1000; }
int one_add(int a, int b) { return a + b + shared_name(1000); }
int abi_add(int a, int b) { return 1000 + a + b; }
