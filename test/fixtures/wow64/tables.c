/* Dispatch pages numbered past the first dispatch table (test/wow64/tables).
 *
 * The run caps every dispatch table at a few hundred pages
 * (GEX_DISPATCH_TABLE_SLOTS), so most pages, the return pad's among them,
 * are numbered in a later table. Three paths cross one:
 *   - ntdll's qsort (native) calls cmp, which has a function-tier unit;
 *   - LoadLibrary runs tablesdll's DllMain on tier-1 (it has no unit), and
 *     its return to native Wine's return pad has to be caught there;
 *   - tablesdll (tier-1) calls twice, a unit function, through a pointer:
 *     the bridge in twice's slot, wherever its page's slots are.
 *
 * Prints "tables ok N" when the array is sorted and the sum is right. */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

typedef int (*cb_t)(int);
typedef int (__stdcall *call_fn)(cb_t, int);

__attribute__((noinline)) int twice(int x) { return 2 * x + 1; }

int __cdecl cmp(const void *a, const void *b) { return *(const int *)a - *(const int *)b; }

int main(void)
{
    int v[256], i;
    for (i = 0; i < 256; i++)
        v[i] = (i * 97) % 256;
    qsort(v, 256, sizeof v[0], cmp);
    for (i = 0; i < 256; i++)
        if (v[i] != i) {
            printf("tables: qsort left v[%d] = %d\n", i, v[i]);
            return 1;
        }
    HMODULE h = LoadLibraryA("tablesdll.dll");
    if (!h) {
        printf("tables: LoadLibrary failed, error %lu\n", GetLastError());
        return 1;
    }
    call_fn call = (call_fn)GetProcAddress(h, "tables_call@8");
    if (!call)
        call = (call_fn)GetProcAddress(h, "tables_call");
    if (!call) {
        printf("tables: no tables_call\n");
        return 1;
    }
    int n = 1000, sum = call(twice, n);
    if (sum != n * n) {
        printf("tables: sum %d, want %d\n", sum, n * n);
        return 1;
    }
    printf("tables ok %d\n", sum);
    return 0;
}
