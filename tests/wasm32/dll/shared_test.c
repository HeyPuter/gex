/* The one-module PE side: abi.dll, one.dll and two.dll as one wasm module
 * behind three skeletons (tools/wine-wasm/pelink.py). */
#include <windows.h>
#include <stdio.h>

int main(void)
{
    HMODULE one = LoadLibraryA("one.dll"), two = LoadLibraryA("two.dll"), abi = LoadLibraryA("abi.dll");
    int fails = 0;
#define CHECK(c) do { int ok_ = (c); printf("%-4s %s\n", ok_ ? "ok" : "FAIL", #c); fails += !ok_; } while (0)
    if (!one || !two || !abi) { printf("LoadLibrary failed\n"); return 1; }
    int (WINAPI *two_call)(int, int) = (void *)GetProcAddress(two, "two_call");
    int (WINAPI *two_shared)(int) = (void *)GetProcAddress(two, "shared_name");
    int (WINAPI *one_abi_add)(int, int) = (void *)GetProcAddress(one, "abi_add");
    int (WINAPI *abi_abi_add)(int, int) = (void *)GetProcAddress(abi, "abi_add");
    int *abi_data = (int *)GetProcAddress(abi, "abi_data");
    CHECK(two_call(2, 3) == 53);           /* one_add(2,3) = 5 (shared_name(1000) = 0) */
    CHECK(two_shared(1) == 3);              /* two.dll's own shared_name, not one.dll's */
    CHECK(one_abi_add(1, 2) == 1003);       /* one.dll's abi_add */
    CHECK(abi_abi_add(1, 2) == 3);          /* abi.dll's abi_add */
    CHECK(abi_data && *abi_data == 1234);   /* a data export pointed at the shared data */
    /* native function pointers (not exports) called by i386 code */
    void *const *(WINAPI *two_procs)(void) = (void *)GetProcAddress(two, "two_procs");
    void *const *procs = two_procs();
    int (WINAPI *p_std)(int, int) = procs[0];
    int (__cdecl *p_cdecl)(int, int) = procs[1];
    long long (WINAPI *p_i64)(long long, int) = procs[2];
    double (__cdecl *p_f64)(double, float) = procs[3];
    struct pt { int x, y; } (WINAPI *p_pt)(int) = procs[4];
    CHECK((ULONG_PTR)procs[0] < 0x3f0000);  /* a table index */
    CHECK(p_std(4, 2) == 42);
    CHECK(p_cdecl(50, 8) == 42);
    CHECK(p_i64(0x100000000ll, 5) == 0x500000000ll);
    CHECK(p_f64(40.5, 1.5f) == 42.0);
    struct pt q = p_pt(7);
    CHECK(q.x == 7 && q.y == -7);
    /* twice in a loop: stdcall's pops and cdecl's must leave ESP right */
    int sum = 0;
    for (int i = 0; i < 1000; i++) sum += p_std(i, 1) - p_cdecl(i, 1);
    CHECK(sum == 1000 * 2 + 9 * 499500);
    printf(fails ? "FAILED\n" : "PASS\n");
    return fails;
}
