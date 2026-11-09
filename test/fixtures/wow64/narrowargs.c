/* Narrow integers (BYTE, char) across the PE boundary, with the upper bits
 * of their register or stack word set, as MSVC leaves them: a BYTE argument
 * loaded with `mov cl, [mem]`, a char result in AL with EAX's other bits
 * whatever they were. Windows reads only the low bits; so must Wine.
 *
 * 1. Into Wine: GetSidLengthRequired and InitializeSid take a BYTE count.
 *    Called through a pointer that declares it a DWORD, the count arrives
 *    with 0x7f0bf000 above its 2.
 * 2. Back from PE code: msvcp140's ctype<char>::toupper calls the facet's
 *    do_toupper through its vtable (slot 6) and returns what it returns. The
 *    do_toupper here returns 'Q' in AL and 0x7f0bf0 above it; Wine's char is
 *    'Q' alone, so the EAX toupper hands back is 0x51.
 * Prints "narrowargs ok" or what differed. */
#include <windows.h>
#include <stdio.h>

typedef DWORD (WINAPI *LEN_FN)(DWORD);
typedef BOOL (WINAPI *INIT_FN)(PSID, PSID_IDENTIFIER_AUTHORITY, DWORD);

/* do_toupper(this, ch): 'Q' with garbage above it, in each convention */
#ifdef _WIN64
typedef DWORD (*TOUPPER_FN)(const void *, DWORD);
__asm__(".globl dirty_toupper\ndirty_toupper:\n\tmovl $0x7f0bf051, %eax\n\tret\n");
#else
typedef DWORD (__attribute__((thiscall)) *TOUPPER_FN)(const void *, DWORD);
__asm__(".globl _dirty_toupper\n_dirty_toupper:\n\tmovl $0x7f0bf051, %eax\n\tret $4\n");
#endif
void dirty_toupper(void);

int main(void)
{
    int bad = 0;
    HMODULE advapi = LoadLibraryA("advapi32.dll");
    LEN_FN len = (LEN_FN)GetProcAddress(advapi, "GetSidLengthRequired");
    INIT_FN init = (INIT_FN)GetProcAddress(advapi, "InitializeSid");
    SID_IDENTIFIER_AUTHORITY nt = SECURITY_NT_AUTHORITY;
    BYTE sid[SECURITY_MAX_SID_SIZE];
    DWORD n = len(0x7f0bf002);
    if (n != 16) { printf("narrowargs: GetSidLengthRequired(2 with high bits) = %lu, not 16\n", n); bad = 1; }
    memset(sid, 0xcc, sizeof(sid));
    if (!init(sid, &nt, 0x7f0bf002) || sid[1] != 2 || !IsValidSid(sid) || GetLengthSid(sid) != 16)
    {
        printf("narrowargs: InitializeSid(count 2 with high bits): count byte %u, valid %d\n", sid[1], IsValidSid(sid));
        bad = 1;
    }

    HMODULE msvcp = LoadLibraryA("msvcp140.dll");
#ifdef _WIN64
    TOUPPER_FN up = (TOUPPER_FN)GetProcAddress(msvcp, "?toupper@?$ctype@D@std@@QEBADD@Z");
#else
    TOUPPER_FN up = (TOUPPER_FN)GetProcAddress(msvcp, "?toupper@?$ctype@D@std@@QBEDD@Z");
#endif
    const void *vtbl[8] = { 0 };
    struct { const void **vtbl; } facet = { vtbl };
    vtbl[6] = (const void *)dirty_toupper;
    if (!up) { printf("narrowargs: no ctype<char>::toupper in msvcp140\n"); bad = 1; }
    else
    {
        DWORD r = up(&facet, 'q');
        if (r != 'Q') { printf("narrowargs: ctype<char>::toupper gave EAX %#lx, not 0x51\n", r); bad = 1; }
    }
    if (!bad) printf("narrowargs ok\n");
    return bad;
}
