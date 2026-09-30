/* uses.exe: the application that pulls the fixture closure together. It
 * imports liba by name and by ordinal (a_ord is NONAME), imports a data
 * export and three forwarders, delay-loads delayed.dll, and registers two TLS
 * callbacks through the CRT's .CRT$XL? table. Linked against the MinGW CRT,
 * so msvcrt.dll and kernel32.dll arrive as system imports. */
#include <stdio.h>
#include <windows.h>
__declspec(dllimport) int a_add(int, int);
__declspec(dllimport) int a_ord(int);
__declspec(dllimport) extern int a_counter;
__declspec(dllimport) int a_fwd_b(int);
__declspec(dllimport) int a_fwd_ord(int);
__declspec(dllimport) void *a_heap(HANDLE, DWORD, SIZE_T);
int d_mul(int, int);
int d_neg(int);

static volatile int tls_calls;
static void NTAPI on_tls_a(PVOID h, DWORD reason, PVOID r) { tls_calls += 1; }
static void NTAPI on_tls_b(PVOID h, DWORD reason, PVOID r) { tls_calls += 10; }
__attribute__((section(".CRT$XLC"), used)) PIMAGE_TLS_CALLBACK tls_cb_a = on_tls_a;
__attribute__((section(".CRT$XLD"), used)) PIMAGE_TLS_CALLBACK tls_cb_b = on_tls_b;

int main(void) {
    int r = a_add(1, 2) + a_ord(3) + a_counter + a_fwd_b(4) + a_fwd_ord(5);
    void *p = a_heap(GetProcessHeap(), 0, 16);
    r += d_mul(6, 7) + d_neg(1);
    printf("%d %p %d\n", r, p, tls_calls);
    return 0;
}
