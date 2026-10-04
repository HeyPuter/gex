/* W0(a): ws2_32's Unix library (dlls/ws2_32/unixlib.c, compiled for wasm32
 * unchanged) driven through its __wine_unix_call_funcs table, as the PE
 * side's WS_CALL would: gethostname, getaddrinfo. Linked statically; the
 * ntdll.so functions it imports are stubbed here. */
#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include "ntstatus.h"
#define WIN32_NO_STATUS
#include "windef.h"
#include "winternl.h"
#define USE_WS_PREFIX
#include "winsock2.h"
#include "ws2tcpip.h"
#include "wine/unixlib.h"
#include "ws2_32_private.h"

extern const unixlib_entry_t __wine_unix_call_funcs[];

/* ntdll.so */
static TEB teb;
TEB *WINAPI NtCurrentTeb(void) { return &teb; }
NTSTATUS WINAPI NtQuerySystemInformation(SYSTEM_INFORMATION_CLASS c, void *p, ULONG l, ULONG *r) { return STATUS_NOT_IMPLEMENTED; }
int __cdecl __wine_dbg_output(const char *str) { return fputs(str, stderr); }
int __cdecl __wine_dbg_header(enum __wine_debug_class cls, struct __wine_debug_channel *ch, const char *fn) { return 0; }

int main(void)
{
    char name[256];
    struct gethostname_params hp = { name, sizeof(name) };
    NTSTATUS st = __wine_unix_call_funcs[ws_unix_gethostname](&hp);
    printf("ws2_32 unix gethostname: status %#x name %s\n", (unsigned)st, st ? "-" : name);

    char buf[4096];
    unsigned int size = sizeof(buf);
    struct WS(addrinfo) hints = { 0 };
    hints.ai_family = WS_AF_INET;
    hints.ai_socktype = WS_SOCK_STREAM;
    struct getaddrinfo_params ap = { "127.0.0.1", "80", &hints, (struct WS(addrinfo) *)buf, &size };
    st = __wine_unix_call_funcs[ws_unix_getaddrinfo](&ap);
    printf("ws2_32 unix getaddrinfo(127.0.0.1, 80): status %#x size %u", (unsigned)st, size);
    if (!st) {
        struct WS(addrinfo) *ai = (struct WS(addrinfo) *)buf;
        struct WS(sockaddr_in) *sin = (struct WS(sockaddr_in) *)ai->ai_addr;
        printf(" family %d addrlen %d port %d addr %08x", ai->ai_family, (int)ai->ai_addrlen,
               ((sin->sin_port & 0xff) << 8) | (sin->sin_port >> 8), (unsigned)sin->sin_addr.WS_s_addr);
    }
    printf("\n");
    return 0;
}
