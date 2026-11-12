/* An aarch64 program calling every AAPCS64 shape in libaconv.so.1
 * (aconv.c): through its exports (gen.py's adapters when the native
 * library replaces it) and through the pointers conv_table() hands out (the
 * GexA64Icall pass's entry thunks), each result checked against the same
 * function compiled here; and a callback of the program's own, called by
 * the library (an exit stub). No libc (raw system calls, its own _start).
 * Prints "FAIL ..." per wrong result, then "ok aconv: N checks" and
 * "N failures". */
#include "aconv.h"

/* aconv.c compiled here as the reference */
#define conv_table r_conv_table
#define table r_table
#define c_i0 r_i0
#define c_i4 r_i4
#define c_i10 r_i10
#define c_mix r_mix
#define c_fp8 r_fp8
#define c_f r_f
#define c_d r_d
#define c_u8 r_u8
#define c_s16 r_s16
#define c_ret_u8 r_ret_u8
#define c_ret_s16 r_ret_s16
#define c_i64 r_i64
#define c_ptr r_ptr
#define c_void r_void
#define c_call r_call
#define c_b4 r_b4
#define c_b8 r_b8
#define c_b12 r_b12
#define c_b16 r_b16
#define c_b24 r_b24
#define c_r8 r_r8
#define c_r16 r_r16
#define c_r24 r_r24
#define c_s10 r_s10
#define c_fp10 r_fp10
#include "aconv.c"
#undef conv_table
#undef c_i0
#undef c_i4
#undef c_i10
#undef c_mix
#undef c_fp8
#undef c_f
#undef c_d
#undef c_u8
#undef c_s16
#undef c_ret_u8
#undef c_ret_s16
#undef c_i64
#undef c_ptr
#undef c_void
#undef c_call
#undef c_b4
#undef c_b8
#undef c_b12
#undef c_b16
#undef c_b24
#undef c_r8
#undef c_r16
#undef c_r24

static long sys3(long n, long a, long b, long c)
{
    register long x8 __asm__("x8") = n, x0 __asm__("x0") = a, x1 __asm__("x1") = b, x2 __asm__("x2") = c;
    __asm__ volatile("svc 0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2) : "memory");
    return x0;
}
static char buf[8192];
static int len, failures, checks;
static void put(const char *s) { while (*s && len < (int)sizeof buf - 64) buf[len++] = *s++; }
static void puti(long long v)
{
    char t[24]; int n = 0; unsigned long long u = v < 0 ? -(unsigned long long)v : v;
    if (v < 0) buf[len++] = '-';
    do t[n++] = '0' + u % 10; while (u /= 10);
    while (n) buf[len++] = t[--n];
}
static void check(int ok, const char *path, const char *what, long long got, long long want)
{
    checks++;
    if (ok) return;
    put("FAIL "); put(path); put(" "); put(what); put(": got "); puti(got); put(", want "); puti(want); put("\n");
    failures++;
}
#define CHK(what, got, want) check((got) == (want), path, what, (long long)(got), (long long)(want))
#define CHKD(what, got, want) check((got) == (want), path, what, (long long)((got) * 1000), (long long)((want) * 1000))

static int guest_cb(int x) { return x * 3; }

/* the pointers of `t`, the library's, against the references */
static void run_table(const char *path, const struct conv_fns *t)
{
    char b[16];
    int out = 0;

    CHK("0 arguments", t->i0(), r_i0());
    CHK("4 int", t->i4(1, 2, 3, 4), r_i4(1, 2, 3, 4));
    CHK("10 int and long long (2 on the stack)", t->i10(1, 1LL << 33, 3, -4, 5, 6LL << 40, 7, 8, 9, -10),
        r_i10(1, 1LL << 33, 3, -4, 5, 6LL << 40, 7, 8, 9, -10));
    CHKD("int, double, int, float, long long, double", t->mix(1, 2.5, 3, 4.25f, 5, 6.125), r_mix(1, 2.5, 3, 4.25f, 5, 6.125));
    CHKD("8 floating point", t->fp8(1.5, 2.5f, 3.5, 4.5f, 5.5, 6.5f, 7.5, 8.5f), r_fp8(1.5, 2.5f, 3.5, 4.5f, 5.5, 6.5f, 7.5, 8.5f));
    CHKD("float in and out", t->f(1.75f), r_f(1.75f));
    CHKD("double in and out", t->d(10.0, 4.0), r_d(10.0, 4.0));
    /* narrow arguments with other bits above them: AAPCS64 leaves them to the callee */
    CHK("unsigned char argument, upper bits set", ((int (*)(unsigned))t->u8)(0xabcdef12u), 0x12);
    CHK("short argument, upper bits set", ((int (*)(unsigned))t->s16)(0x12348001u), -32767);
    CHK("unsigned char result", t->ret_u8(0x1fe), r_ret_u8(0x1fe));
    CHK("short result", t->ret_s16(-40000), r_ret_s16(-40000));
    CHK("long long", t->i64(1LL << 50, -7), r_i64(1LL << 50, -7));
    CHK("pointer in and out", t->ptr(b, 5) - b, 5);
    t->vd(&out, 6);
    CHK("void result", out, 30);
    CHK("a callback of the program's, called by the library", t->call(guest_cb, 14), 43);
    CHK("10 long long and int (2 on the stack), not exported", t->s10(1, -2, 3LL << 34, 4, 5, -6, 7, 8, 9LL << 36, 10),
        r_s10(1, -2, 3LL << 34, 4, 5, -6, 7, 8, 9LL << 36, 10));
    CHKD("10 floating point (2 on the stack), not exported", t->fp10(1.5, 2.5f, 3.5, 4.5f, 5.5, 6.5f, 7.5, 8.5f, 9.5, 10.5f),
         r_fp10(1.5, 2.5f, 3.5, 4.5f, 5.5, 6.5f, 7.5, 8.5f, 9.5, 10.5f));
}

/* the exports, called directly */
static void run_exports(void)
{
    const char *path = "export";
    s4 b4 = { 70000 }; s8 b8 = { 11, -12 }; s12 b12 = { { 5, 6, -7 } }; s16 b16 = { 1LL << 40, -3 };
    s24 b24 = { { 9, -(1LL << 35), 77 } };
    char b[16];
    int out = 0;

    CHK("0 arguments", c_i0(), r_i0());
    CHK("4 int", c_i4(1, 2, 3, 4), r_i4(1, 2, 3, 4));
    CHK("10 int and long long (2 on the stack)", c_i10(1, 1LL << 33, 3, -4, 5, 6LL << 40, 7, 8, 9, -10),
        r_i10(1, 1LL << 33, 3, -4, 5, 6LL << 40, 7, 8, 9, -10));
    CHKD("int, double, int, float, long long, double", c_mix(1, 2.5, 3, 4.25f, 5, 6.125), r_mix(1, 2.5, 3, 4.25f, 5, 6.125));
    CHKD("8 floating point", c_fp8(1.5, 2.5f, 3.5, 4.5f, 5.5, 6.5f, 7.5, 8.5f), r_fp8(1.5, 2.5f, 3.5, 4.5f, 5.5, 6.5f, 7.5, 8.5f));
    CHKD("float in and out", c_f(1.75f), r_f(1.75f));
    CHKD("double in and out", c_d(10.0, 4.0), r_d(10.0, 4.0));
    CHK("unsigned char argument, upper bits set", ((int (*)(unsigned))c_u8)(0xabcdef12u), 0x12);
    CHK("short argument, upper bits set", ((int (*)(unsigned))c_s16)(0x12348001u), -32767);
    CHK("unsigned char result", c_ret_u8(0x1fe), r_ret_u8(0x1fe));
    CHK("short result", c_ret_s16(-40000), r_ret_s16(-40000));
    CHK("long long", c_i64(1LL << 50, -7), r_i64(1LL << 50, -7));
    CHK("pointer in and out", c_ptr(b, 5) - b, 5);
    c_void(&out, 6);
    CHK("void result", out, 30);
    CHK("a callback of the program's, called by the library", c_call(guest_cb, 14), 43);
    CHK("4-byte struct by value", c_b4(1, b4, 2), r_b4(1, b4, 2));
    CHK("8-byte struct by value", c_b8(1, b8, 2), r_b8(1, b8, 2));
    CHK("12-byte struct by value (2 registers)", c_b12(1, b12, 2), r_b12(1, b12, 2));
    CHK("16-byte struct by value (2 registers)", c_b16(1, b16, 2), r_b16(1, b16, 2));
    CHK("24-byte struct (by reference)", c_b24(1, b24, 2), r_b24(1, b24, 2));
    { s8 g = c_r8(100), w = r_r8(100); CHK("8-byte struct result (x0)", g.a * 1000 + g.b, w.a * 1000 + w.b); }
    { s16 g = c_r16(20), w = r_r16(20); CHK("16-byte struct result (x0, x1)", g.a + g.b, w.a + w.b); }
    { s24 g = c_r24(-3, 5), w = r_r24(-3, 5); CHK("24-byte struct result (x8)", g.a[0] + g.a[1] * 3 + g.a[2], w.a[0] + w.a[1] * 3 + w.a[2]); }
}

static int run(void)
{
    run_table("pointer", conv_table());
    run_exports();
    put("ok aconv: "); puti(checks); put(" checks\n"); puti(failures); put(" failures\n");
    sys3(64, 1, (long)buf, len);
    return failures != 0;
}

__attribute__((used)) static void start_c(void) { sys3(93, run(), 0, 0); for (;;) ; }
__asm__(".globl _start\n_start:\n\tmov x29, #0\n\tmov x30, #0\n\tbl start_c\n");
