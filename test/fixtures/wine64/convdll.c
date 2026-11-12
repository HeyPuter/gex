/* conv.h's functions, natively (gexcc-win): each result a checksum of
 * every argument, so a wrong slot, register or width shows. conv_table()
 * hands out a table of pointers to them (table indices, which guest code
 * calls as addresses); convdll.def exports each by its own name too. */
#include "conv.h"

int c_i0(void) { return 7; }
int c_i4(int a, int b, int c, int d) { return a + 2 * b + 3 * c + 4 * d; }
int c_i6(int a, int b, int c, int d, int e, int f) { return a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f; }
long long c_i12(int a, long long b, int c, long long d, int e, long long f, int g, long long h, int i, long long j, int k, long long l)
{
    return a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f + 7 * g + 8 * h + 9 * i + 10 * j + 11 * k + 12 * l;
}
double c_mix(int a, double b, int c, float d, long long e, double f) { return a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f; }
double c_fp8(double a, float b, double c, float d, double e, float f, double g, float h)
{
    return a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f + 7 * g + 8 * h;
}
float c_f(float a) { return a * 2.5f; }
double c_d(double a, double b) { return a / b; }
/* (the guest passes these widened with other bits above: the callee may
 * read only the narrow value) */
int c_u8(unsigned char a) { return a; }
int c_s16(short a) { return a; }
unsigned char c_ret_u8(int a) { return (unsigned char)(a + 1); }
short c_ret_s16(int a) { return (short)(a - 1); }
long long c_i64(long long a, long long b) { return a * 3 - b; }
char *c_ptr(char *p, int n) { return p + n; }
int c_b1(int t, struct s1 v, int u) { return t + 2 * v.a + 3 * u; }
int c_b2(int t, struct s2 v, int u) { return t + 2 * v.a + 3 * u; }
int c_b3(int t, struct s3 v, int u) { return t + 2 * v.a[0] + 3 * v.a[1] + 4 * v.a[2] + 5 * u; }
int c_b4(int t, struct s4 v, int u) { return t + 2 * v.a + 3 * u; }
int c_b8(int t, struct s8 v, int u) { return t + 2 * v.a + 3 * v.b + 4 * u; }
int c_b12(int t, struct s12 v, int u) { return t + 2 * v.a[0] + 3 * v.a[1] + 4 * v.a[2] + 5 * u; }
long long c_b16(int t, struct s16 v, int u) { return t + 2 * v.a + 3 * v.b + 4 * u; }
long long c_b24(int t, struct s24 v, int u) { return t + 2 * v.a[0] + 3 * v.a[1] + 4 * v.a[2] + 5 * u; }
struct s1 c_r1(int x) { struct s1 r = { (char)(x + 1) }; return r; }
struct s2 c_r2(int x) { struct s2 r = { (short)(x + 2) }; return r; }
struct s4 c_r4(int x) { struct s4 r = { x + 4 }; return r; }
struct s8 c_r8(int x) { struct s8 r = { x + 8, x - 8 }; return r; }
struct s12 c_r12(int x) { struct s12 r = { { x + 1, x + 2, x + 3 } }; return r; }
struct s24 c_r24(int x, int y) { struct s24 r = { { x, (long long)y << 33, (long long)x * y } }; return r; }
void c_void(int *out, int v) { *out = v * 5; }
/* a guest function called back from native code */
int c_call(int (*cb)(int), int x) { return cb(x) + 1; }
int c_i16(int a, int b, int c, int d, int e, int f, int g, int h, int i, int j, int k, int l, int m, int n, int o, int p)
{
    return a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f + 7 * g + 8 * h + 9 * i + 10 * j + 11 * k + 12 * l + 13 * m + 14 * n + 15 * o + 16 * p;
}

#define CONV_PTR(r, n, a) n,
static const struct conv_table table = { CONV_FUNCS(CONV_PTR) };
const struct conv_table *conv_table(void) { return &table; }

/* native to native through a pointer, n times (icallbench's reference):
 * the pointer read through a volatile, so the call stays indirect */
static int (*volatile self_fn)(int, int, int, int) = c_i4;
int conv_selfloop(int n)
{
    int s = 0;
    for (int i = 0; i < n; i++) s += self_fn(i, 1, 2, 3);
    return s;
}
