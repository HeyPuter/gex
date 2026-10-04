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
    printf(fails ? "FAILED\n" : "PASS\n");
    return fails;
}
