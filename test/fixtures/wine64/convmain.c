/* Guest x86-64 code calling native functions of every Win64 shape
 * (convdll.c): through the pointers conv_table() hands out (the native
 * module's entry path: a guest call to a table index) and through
 * GetProcAddress (the export path). Each result is checked against the
 * same function compiled here, as x86. Then a guest callback called from
 * native code. Prints "ok ..." per check and "N failures". */
#include <windows.h>
#include <stdio.h>
#include <string.h>

/* convdll.c's functions compiled here as the reference */
#define conv_table ref_conv_table
#define conv_selfloop ref_conv_selfloop
#define table ref_table
#define self_fn ref_self_fn
#define c_i0 r_i0
#define c_i4 r_i4
#define c_i6 r_i6
#define c_i12 r_i12
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
#define c_b1 r_b1
#define c_b2 r_b2
#define c_b3 r_b3
#define c_b4 r_b4
#define c_b8 r_b8
#define c_b12 r_b12
#define c_b16 r_b16
#define c_b24 r_b24
#define c_r1 r_r1
#define c_r2 r_r2
#define c_r4 r_r4
#define c_r8 r_r8
#define c_r12 r_r12
#define c_r24 r_r24
#define c_void r_void
#define c_call r_call
#define c_i16 r_i16
#include "convdll.c"

static int failures, checks;
static void check(int ok, const char *path, const char *what, long long got, long long want)
{
    checks++;
    if (ok) return;
    printf("FAIL %s %s: got %lld (0x%llx), want %lld\n", path, what, got, (unsigned long long)got, want);
    failures++;
}
#define CHK(what, got, want) check((got) == (want), path, what, (long long)(got), (long long)(want))
#define CHKD(what, got, want) check((got) == (want), path, what, (long long)((got) * 1000), (long long)((want) * 1000))

static int guest_cb(int x) { return x * 3; }

/* every function of `t` (a table of native pointers) against the references */
static void run(const char *path, const struct conv_table *t)
{
    struct s1 b1 = { -5 }; struct s2 b2 = { -300 }; struct s3 b3 = { { 1, -2, 3 } }; struct s4 b4 = { 70000 };
    struct s8 b8 = { 11, -12 }; struct s12 b12 = { { 5, 6, -7 } }; struct s16 b16 = { 1LL << 40, -3 };
    struct s24 b24 = { { 9, -(1LL << 35), 77 } };
    char buf[16];
    int out = 0;

    CHK("0 arguments", t->r_i0(), r_i0());
    CHK("4 int", t->r_i4(1, 2, 3, 4), r_i4(1, 2, 3, 4));
    CHK("6 int (2 on the stack)", t->r_i6(1, -2, 3, -4, 5, -6), r_i6(1, -2, 3, -4, 5, -6));
    CHK("12 int and long long", t->r_i12(1, 1LL << 33, 3, -4, 5, 6LL << 40, 7, 8, 9, -10, 11, 12),
        r_i12(1, 1LL << 33, 3, -4, 5, 6LL << 40, 7, 8, 9, -10, 11, 12));
    CHK("16 int", t->r_i16(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16),
        r_i16(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16));
    CHKD("int, double, int, float, long long, double", t->r_mix(1, 2.5, 3, 4.25f, 5, 6.125), r_mix(1, 2.5, 3, 4.25f, 5, 6.125));
    CHKD("8 floating point (4 on the stack)", t->r_fp8(1.5, 2.5f, 3.5, 4.5f, 5.5, 6.5f, 7.5, 8.5f),
         r_fp8(1.5, 2.5f, 3.5, 4.5f, 5.5, 6.5f, 7.5, 8.5f));
    CHKD("float in and out", t->r_f(1.75f), r_f(1.75f));
    CHKD("double in and out", t->r_d(10.0, 4.0), r_d(10.0, 4.0));
    /* narrow arguments with other bits above them, as MSVC may pass them */
    CHK("unsigned char argument, upper bits set", ((int (*)(unsigned))t->r_u8)(0xabcdef12u), 0x12);
    CHK("short argument, upper bits set", ((int (*)(unsigned))t->r_s16)(0x12348001u), -32767);
    CHK("unsigned char result", t->r_ret_u8(0x1fe), r_ret_u8(0x1fe));
    CHK("short result", t->r_ret_s16(-40000), r_ret_s16(-40000));
    CHK("long long", t->r_i64(1LL << 50, -7), r_i64(1LL << 50, -7));
    CHK("pointer in and out", t->r_ptr(buf, 5) - buf, 5);
    CHK("1-byte struct by value", t->r_b1(1, b1, 2), r_b1(1, b1, 2));
    CHK("2-byte struct by value", t->r_b2(1, b2, 2), r_b2(1, b2, 2));
    CHK("3-byte struct (by reference)", t->r_b3(1, b3, 2), r_b3(1, b3, 2));
    CHK("4-byte struct by value", t->r_b4(1, b4, 2), r_b4(1, b4, 2));
    CHK("8-byte struct by value", t->r_b8(1, b8, 2), r_b8(1, b8, 2));
    CHK("12-byte struct (by reference)", t->r_b12(1, b12, 2), r_b12(1, b12, 2));
    CHK("16-byte struct (by reference)", t->r_b16(1, b16, 2), r_b16(1, b16, 2));
    CHK("24-byte struct (by reference)", t->r_b24(1, b24, 2), r_b24(1, b24, 2));
    CHK("1-byte struct result (in RAX)", t->r_r1(9).a, r_r1(9).a);
    CHK("2-byte struct result (in RAX)", t->r_r2(-9).a, r_r2(-9).a);
    CHK("4-byte struct result (in RAX)", t->r_r4(9).a, r_r4(9).a);
    { struct s8 g = t->r_r8(100), w = r_r8(100); CHK("8-byte struct result (in RAX)", g.a * 1000 + g.b, w.a * 1000 + w.b); }
    { struct s12 g = t->r_r12(20), w = r_r12(20); CHK("12-byte struct result (hidden pointer)", memcmp(&g, &w, sizeof g), 0); }
    { struct s24 g = t->r_r24(-3, 5), w = r_r24(-3, 5); CHK("24-byte struct result (hidden pointer)", memcmp(&g, &w, sizeof g), 0); }
    t->r_void(&out, 6);
    CHK("void result", out, 30);
    CHK("a guest callback called by native code", t->r_call(guest_cb, 14), 43);
}

int main(void)
{
    HMODULE m = LoadLibraryA("convdll.dll");
    const struct conv_table *(*get)(void);
    struct conv_table e;

    if (!m || !(get = (void *)GetProcAddress(m, "conv_table")))
    {
        printf("FAIL convdll.dll: %lu\n1 failures\n", GetLastError());
        return 1;
    }
    run("pointer", get());
    /* the same functions through their exports */
    {
        static const char *const names[] = { "c_i0", "c_i4", "c_i6", "c_i12", "c_mix", "c_fp8", "c_f", "c_d", "c_u8", "c_s16",
            "c_ret_u8", "c_ret_s16", "c_i64", "c_ptr", "c_b1", "c_b2", "c_b3", "c_b4", "c_b8", "c_b12", "c_b16", "c_b24",
            "c_r1", "c_r2", "c_r4", "c_r8", "c_r12", "c_r24", "c_void", "c_call", "c_i16" };
        void **slot = (void **)&e;
        for (unsigned i = 0; i < sizeof names / sizeof *names; i++)
            if (!(slot[i] = (void *)GetProcAddress(m, names[i]))) { printf("FAIL no export %s\n", names[i]); failures++; }
    }
    if (!failures) run("export", &e);
    printf("ok convmatrix: %d checks\n%d failures\n", checks, failures);
    fflush(stdout);
    return failures != 0;
}
