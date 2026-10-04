/* W1: an i386 program calling abi.dll (abi.c as a wasm module behind a
 * winebuild skeleton) through every kind of entry adapter. */
#include <windows.h>
#include <stdio.h>

typedef struct { int x, y; } pair;
static int __cdecl cb(int x) { return x * 3; }
static int WINAPI cb_std(int x, int y) { return x + y; }

int main(void)
{
    HMODULE h = LoadLibraryA("abi.dll");
    int fails = 0;
#define GET(t, n) __typeof__(t) n = (__typeof__(t))GetProcAddress(h, #n)
#define CHECK(c) do { int ok_ = (c); printf("%-4s %s\n", ok_ ? "ok" : "FAIL", #c); fails += !ok_; } while (0)
    if (!h) { printf("LoadLibrary failed %lu\n", GetLastError()); return 1; }
    GET(int (WINAPI *)(int, int), abi_add);
    GET(int (__cdecl *)(int, int), abi_sub);
    GET(long long (WINAPI *)(long long, int), abi_mul64);
    GET(double (__cdecl *)(double, float, double), abi_fma);
    GET(float (WINAPI *)(float), abi_half);
    GET(int (__thiscall *)(int *, int), abi_this);
    GET(int (__fastcall *)(int, int, int), abi_fast);
    GET(int (WINAPI *)(LARGE_INTEGER), abi_large);
    GET(pair (WINAPI *)(int, int), abi_pair);
    GET(void (WINAPI *)(int *), abi_void);
    GET(int (__cdecl *)(int (__cdecl *)(int), int), abi_callback);
    GET(int (WINAPI *)(int (WINAPI *)(int, int), int), abi_callback_std);
    GET(int *, abi_data);
    GET(DWORD (WINAPI *)(void), abi_forward);
    int self = 6, v = 0;
    LARGE_INTEGER li;
    li.u.LowPart = 5; li.u.HighPart = 7;
    CHECK(abi_add(40, 2) == 42);
    CHECK(abi_sub(40, 2) == 38);
    CHECK(abi_mul64(0x100000000ll, 3) == 0x300000000ll);
    CHECK(abi_fma(1.5, 2.0f, 0.25) == 3.25);
    CHECK(abi_half(5.0f) == 2.5f);
    CHECK(abi_this(&self, 7) == 42);
    CHECK(abi_fast(1, 2, 3) == 123);
    CHECK(abi_large(li) == 7005);
    pair p = abi_pair(10, 20);
    CHECK(p.x == 11 && p.y == 22);
    abi_void(&v);
    CHECK(v == 77);
    CHECK(abi_callback(cb, 5) == 16);
    CHECK(abi_callback_std(cb_std, 4) == 14);
    CHECK(abi_data && *abi_data == 1234);
    CHECK(abi_forward != NULL && abi_forward() == GetTickCount() || abi_forward() + 1000 > GetTickCount());
    CHECK(GetProcAddress(h, "abi_missing") != NULL);
    printf(fails ? "FAILED\n" : "PASS\n");
    return fails;
}
