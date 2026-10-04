/* W1: one function per kind of entry adapter (abi.spec), compiled to
 * wasm32 as Wine's PE-side code is (-D_WIN32, the ICALL pass). */
typedef struct { int x, y; } pair;
typedef union { struct { unsigned int lo; int hi; } u; long long q; } large;

int abi_add(int a, int b) { return a + b; }
int abi_sub(int a, int b) { return a - b; }
long long abi_mul64(long long a, int b) { return a * b; }
double abi_fma(double a, float b, double c) { return a * b + c; }
float abi_half(float x) { return x / 2; }
int abi_this(int *self, int k) { return *self * k; }
int abi_fast(int a, int b, int c) { return a * 100 + b * 10 + c; }
int abi_large(large v) { return v.u.hi * 1000 + (int)v.u.lo; }
pair abi_pair(int x, int y) { pair p = { x + 1, y + 2 }; return p; }
void abi_void(int *out) { *out = 77; }
int abi_callback(int (*f)(int), int x) { return f(x) + 1; }
int abi_callback_std(int (*f)(int, int), int x) { return f(x, 3) * 2; }
int abi_data = 1234;

/* winecrt0's, for the stub: here it records the call. */
int abi_stub_called;
void __wine_spec_unimplemented_stub(const char *module, const char *function) { abi_stub_called = 1; }
