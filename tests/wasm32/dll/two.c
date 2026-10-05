/* two.dll: calls one.dll's export directly (one module, one call). */
int one_add(int a, int b);
static int helper(int x) { return x * 2; }
int shared_name(int x) { return helper(x) + 1; }
int two_call(int a, int b) { return one_add(a, b) * 10 + shared_name(1); }

/* Native function pointers handed to i386 code (a window class's
 * procedure, a COM vtable): the ICALL pass's entry thunks. WINAPI is this
 * annotation on wasm32 (corecrt.h); these are not exports. */
#define STDCALL __attribute__((annotate("gex.stdcall")))
typedef struct { int x, y; } pt;
static int STDCALL proc_std(int a, int b) { return a * 10 + b; }
static int proc_cdecl(int a, int b) { return a - b; }
static long long STDCALL proc_i64(long long a, int b) { return a * b; }
static double proc_f64(double a, float b) { return a + b; }
static pt STDCALL proc_pt(int x) { pt p = { x, -x }; return p; }
static void *const procs[] = { proc_std, proc_cdecl, proc_i64, proc_f64, proc_pt };
void *const *two_procs(void) { return procs; }
