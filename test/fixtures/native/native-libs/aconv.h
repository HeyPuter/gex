/* Calls of every AAPCS64 shape into a native library (aconv.c) from an
 * aarch64 program (aconvt.c): CONV_API functions are exports (gen.py's
 * adapters); conv_table() hands out pointers to them and to two that are
 * not exported (stack arguments of both kinds), the GexA64Icall pass's
 * entry thunks. Freestanding: no libc either side. */
#ifndef ACONV_H
#define ACONV_H
#define CONV_API __attribute__((visibility("default")))

typedef struct { int a; } s4;
typedef struct { int a, b; } s8;
typedef struct { int a[3]; } s12;
typedef struct { long long a; int b; } s16;
typedef struct { long long a[3]; } s24;

CONV_API int c_i0(void);
CONV_API int c_i4(int a, int b, int c, int d);
CONV_API long long c_i10(int a, long long b, int c, long long d, int e, long long f, int g, long long h, int i, long long j);
CONV_API double c_mix(int a, double b, int c, float d, long long e, double f);
CONV_API double c_fp8(double a, float b, double c, float d, double e, float f, double g, float h);
CONV_API float c_f(float a);
CONV_API double c_d(double a, double b);
CONV_API int c_u8(unsigned char a);
CONV_API int c_s16(short a);
CONV_API unsigned char c_ret_u8(int a);
CONV_API short c_ret_s16(int a);
CONV_API long long c_i64(long long a, long long b);
CONV_API char *c_ptr(char *p, int n);
CONV_API void c_void(int *out, int a);
CONV_API int c_call(int (*cb)(int), int x);
CONV_API int c_b4(int x, s4 s, int y);
CONV_API int c_b8(int x, s8 s, int y);
CONV_API int c_b12(int x, s12 s, int y);
CONV_API long long c_b16(int x, s16 s, int y);
CONV_API long long c_b24(int x, s24 s, int y);
CONV_API s8 c_r8(int x);
CONV_API s16 c_r16(int x);
CONV_API s24 c_r24(int x, int y);
/* (not exported: their pointers only) */
long long c_s10(long long a, int b, long long c, int d, long long e, int f, long long g, int h, long long i, int j);
double c_fp10(double a, float b, double c, float d, double e, float f, double g, float h, double i, float j);

struct conv_fns
{
    int (*i0)(void);
    int (*i4)(int, int, int, int);
    long long (*i10)(int, long long, int, long long, int, long long, int, long long, int, long long);
    double (*mix)(int, double, int, float, long long, double);
    double (*fp8)(double, float, double, float, double, float, double, float);
    float (*f)(float);
    double (*d)(double, double);
    int (*u8)(unsigned char);
    int (*s16)(short);
    unsigned char (*ret_u8)(int);
    short (*ret_s16)(int);
    long long (*i64)(long long, long long);
    char *(*ptr)(char *, int);
    void (*vd)(int *, int);
    int (*call)(int (*)(int), int);
    long long (*s10)(long long, int, long long, int, long long, int, long long, int, long long, int);
    double (*fp10)(double, float, double, float, double, float, double, float, double, float);
};
CONV_API const struct conv_fns *conv_table(void);
#endif
