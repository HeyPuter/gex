/* A program's statics and a native builtin's static data are memory Wine
 * knows: VirtualQuery says committed and writable, and a server call's reply
 * lands in them. Under gex a native module's data is guest memory gex maps
 * itself (gex.host_data), outside Wine's images; built with gexcc-win this
 * program's own statics are such memory too (plain MinGW: its image).
 *
 *   own bss, own data   VirtualQuery MEM_COMMIT and writable; FSCTL_PIPE_PEEK
 *                       (a server ioctl with a reply) into each, as into a
 *                       stack buffer
 *   msvcrt _iob         a native builtin's data (the core's): VirtualQuery
 *                       MEM_COMMIT and writable, IsBadWritePtr FALSE
 *
 * Prints "ok ..." per check and "N failures".
 * gex-nt-arch: x64
 */
#include <windows.h>
#include <winternl.h>
#include <winioctl.h>
#include <stdio.h>
#include <string.h>

#ifndef FSCTL_PIPE_PEEK
#define FSCTL_PIPE_PEEK CTL_CODE(FILE_DEVICE_NAMED_PIPE, 3, METHOD_BUFFERED, FILE_READ_DATA)
#endif

typedef struct { ULONG NamedPipeState; ULONG ReadDataAvailable; ULONG NumberOfMessages; ULONG MessageLength; char Data[1]; } PEEK;
static int failures;
static char own_bss[256];
static char own_data[256] = "x";

static void query(const char *what, const void *p)
{
    MEMORY_BASIC_INFORMATION m;
    SIZE_T n;
    int good;

    memset(&m, 0, sizeof(m));
    n = VirtualQuery(p, &m, sizeof(m));
    good = n == sizeof(m) && m.State == MEM_COMMIT &&
           (m.Protect & (PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY));
    printf("%s VirtualQuery %s: %p state %lx protect %lx type %lx\n", good ? "ok" : "FAIL", what, p,
           (unsigned long)m.State, (unsigned long)m.Protect, (unsigned long)m.Type);
    if (!good) failures++;
}

static void peek(const char *what, HANDLE srv, char *buf, size_t size)
{
    typedef NTSTATUS (WINAPI *fsctl_fn)(HANDLE, HANDLE, void *, void *, IO_STATUS_BLOCK *, ULONG, void *, ULONG, void *, ULONG);
    fsctl_fn fsctl = (fsctl_fn)GetProcAddress(GetModuleHandleA("ntdll.dll"), "NtFsControlFile");
    IO_STATUS_BLOCK io;
    PEEK *pk = (PEEK *)buf;
    NTSTATUS st;
    int good;

    memset(buf, 0, size);
    st = fsctl(srv, NULL, NULL, NULL, &io, FSCTL_PIPE_PEEK, NULL, 0, buf, (ULONG)size);
    good = st == 0 && pk->ReadDataAvailable == 5 && !memcmp(pk->Data, "hello", 5);
    printf("%s FSCTL_PIPE_PEEK into %s: status %lx, %lu bytes\n", good ? "ok" : "FAIL", what,
           (unsigned long)st, (unsigned long)pk->ReadDataAvailable);
    if (!good) failures++;
}

int main(int argc, char **argv)
{
    HMODULE msvcrt = LoadLibraryA("msvcrt.dll");
    void *iob = msvcrt ? (void *)GetProcAddress(msvcrt, "_iob") : NULL;
    char stack_buf[256];
    HANDLE srv, cli;
    DWORD w;

    setvbuf(stdout, NULL, _IONBF, 0);
    query("own bss", own_bss);
    query("own data", own_data);
    if (iob)
    {
        query("msvcrt _iob", iob);
        printf("%s IsBadWritePtr msvcrt _iob\n", IsBadWritePtr(iob, 16) ? "FAIL" : "ok");
        if (IsBadWritePtr(iob, 16)) failures++;
    }
    else { printf("FAIL no msvcrt _iob\n"); failures++; }

    srv = CreateNamedPipeA("\\\\.\\pipe\\gex-hostdata", PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE | PIPE_WAIT, 1, 4096, 4096, 0, NULL);
    cli = CreateFileA("\\\\.\\pipe\\gex-hostdata", GENERIC_READ | GENERIC_WRITE, 0, NULL, OPEN_EXISTING, 0, NULL);
    if (srv == INVALID_HANDLE_VALUE || cli == INVALID_HANDLE_VALUE || !WriteFile(cli, "hello", 5, &w, NULL))
    {
        printf("FAIL pipe: %lu\n", GetLastError());
        failures++;
    }
    else
    {
        peek("a stack buffer", srv, stack_buf, sizeof(stack_buf));
        peek("own bss", srv, own_bss, sizeof(own_bss));
        peek("own data", srv, own_data, sizeof(own_data));
    }
    printf("%d failures\n", failures);
    return failures ? 1 : 0;
}
