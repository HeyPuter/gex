/* What convdll.c (a gexcc-win DLL: native wasm) hands guest x86-64 code
 * (convmain.c, lateload.c, cbthrow.c, icallbench.c, built with MinGW):
 * function pointers to functions of every shape the Win64 convention has,
 * which guest code calls as addresses (the native side's entry path), and
 * the same functions exported, which guest code calls through
 * GetProcAddress (the export path). Both sides compile this header. */
#include <stdint.h>

struct s1 { char a; };
struct s2 { short a; };
struct s3 { char a[3]; };
struct s4 { int a; };
struct s8 { int a, b; };
struct s12 { int a[3]; };
struct s16 { long long a, b; };
struct s24 { long long a[3]; };

/* the functions, each by a name of the table's and an export of its own */
#define CONV_FUNCS(X) \
    X(int, c_i0, (void)) \
    X(int, c_i4, (int a, int b, int c, int d)) \
    X(int, c_i6, (int a, int b, int c, int d, int e, int f)) \
    X(long long, c_i12, (int a, long long b, int c, long long d, int e, long long f, int g, long long h, int i, long long j, int k, long long l)) \
    X(double, c_mix, (int a, double b, int c, float d, long long e, double f)) \
    X(double, c_fp8, (double a, float b, double c, float d, double e, float f, double g, float h)) \
    X(float, c_f, (float a)) \
    X(double, c_d, (double a, double b)) \
    X(int, c_u8, (unsigned char a)) \
    X(int, c_s16, (short a)) \
    X(unsigned char, c_ret_u8, (int a)) \
    X(short, c_ret_s16, (int a)) \
    X(long long, c_i64, (long long a, long long b)) \
    X(char *, c_ptr, (char *p, int n)) \
    X(int, c_b1, (int t, struct s1 v, int u)) \
    X(int, c_b2, (int t, struct s2 v, int u)) \
    X(int, c_b3, (int t, struct s3 v, int u)) \
    X(int, c_b4, (int t, struct s4 v, int u)) \
    X(int, c_b8, (int t, struct s8 v, int u)) \
    X(int, c_b12, (int t, struct s12 v, int u)) \
    X(long long, c_b16, (int t, struct s16 v, int u)) \
    X(long long, c_b24, (int t, struct s24 v, int u)) \
    X(struct s1, c_r1, (int x)) \
    X(struct s2, c_r2, (int x)) \
    X(struct s4, c_r4, (int x)) \
    X(struct s8, c_r8, (int x)) \
    X(struct s12, c_r12, (int x)) \
    X(struct s24, c_r24, (int x, int y)) \
    X(void, c_void, (int *out, int v)) \
    X(int, c_call, (int (*cb)(int), int x)) \
    X(int, c_i16, (int a, int b, int c, int d, int e, int f, int g, int h, int i, int j, int k, int l, int m, int n, int o, int p))

#define CONV_FIELD(r, n, a) r (*n) a;
struct conv_table { CONV_FUNCS(CONV_FIELD) };
