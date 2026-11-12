/* aconv.h's functions: each mixes every argument into its result, so a
 * misplaced one shows. Built as the native library (wasm64, build-aconv.sh),
 * as the arm64 library the program links against, and into the program as
 * the reference. */
#include "aconv.h"

int c_i0(void) { return 4242; }
int c_i4(int a, int b, int c, int d) { return a + 2 * b + 3 * c + 4 * d; }
long long c_i10(int a, long long b, int c, long long d, int e, long long f, int g, long long h, int i, long long j)
{
    return a + 3 * b + 5 * c + 7 * d + 11 * e + 13 * f + 17 * g + 19 * h + 23 * i + 29 * j;
}
double c_mix(int a, double b, int c, float d, long long e, double f) { return a + b * 3 + c * 5 + d * 7 + e * 11 + f * 13; }
double c_fp8(double a, float b, double c, float d, double e, float f, double g, float h)
{
    return a + b * 2 + c * 3 + d * 4 + e * 5 + f * 6 + g * 7 + h * 8;
}
float c_f(float a) { return a * 2.5f; }
double c_d(double a, double b) { return a / b; }
int c_u8(unsigned char a) { return a; }
int c_s16(short a) { return a; }
unsigned char c_ret_u8(int a) { return (unsigned char)(a + 1); }
short c_ret_s16(int a) { return (short)(a - 1); }
long long c_i64(long long a, long long b) { return a * 3 - b; }
char *c_ptr(char *p, int n) { return p + n; }
void c_void(int *out, int a) { *out = a * 5; }
int c_call(int (*cb)(int), int x) { return cb(x) + 1; }
int c_b4(int x, s4 s, int y) { return x + s.a * 3 + y * 5; }
int c_b8(int x, s8 s, int y) { return x + s.a * 3 + s.b * 7 + y * 11; }
int c_b12(int x, s12 s, int y) { return x + s.a[0] * 3 + s.a[1] * 7 + s.a[2] * 13 + y * 17; }
long long c_b16(int x, s16 s, int y) { return x + s.a * 3 + s.b * 7 + y * 11; }
long long c_b24(int x, s24 s, int y) { return x + s.a[0] * 3 + s.a[1] * 7 + s.a[2] * 13 + y * 17; }
s8 c_r8(int x) { s8 r = { x, -x * 2 }; return r; }
s16 c_r16(int x) { s16 r = { (long long)x << 33, x + 1 }; return r; }
s24 c_r24(int x, int y) { s24 r = { { x, y, (long long)x * y << 20 } }; return r; }
long long c_s10(long long a, int b, long long c, int d, long long e, int f, long long g, int h, long long i, int j)
{
    return a * 2 + b * 3 + c * 5 + d * 7 + e * 11 + f * 13 + g * 17 + h * 19 + i * 23 + j * 31;
}
double c_fp10(double a, float b, double c, float d, double e, float f, double g, float h, double i, float j)
{
    return a + b * 2 + c * 3 + d * 4 + e * 5 + f * 6 + g * 7 + h * 8 + i * 9 + j * 10;
}

static const struct conv_fns table = {
    c_i0, c_i4, c_i10, c_mix, c_fp8, c_f, c_d, c_u8, c_s16, c_ret_u8, c_ret_s16, c_i64, c_ptr, c_void, c_call,
    c_s10, c_fp10,
};
const struct conv_fns *conv_table(void) { return &table; }
