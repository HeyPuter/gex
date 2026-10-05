/* Anonymous pipes (CreatePipe: a named pipe in this process plus its client
 * end), a named pipe's client open, data both ways, end of file when the
 * writer closes, and a blocked reader woken by a writer thread. Unity's
 * players create a pipe at startup; with no NtCreateNamedPipeFile, CreatePipe
 * looped forever. Also SystemSupportedProcessorArchitectures. Prints one
 * line per check.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

static HANDLE late_w;
static DWORD WINAPI write_later(void *p) {
    Sleep(50);
    DWORD n;
    WriteFile(late_w, "late", 4, &n, NULL);
    return 0;
}

typedef NTSTATUS (WINAPI *QSIEx)(ULONG, void *, ULONG, void *, ULONG, ULONG *);

int main(void) {
    HANDLE r, w;
    CHECK("CreatePipe", CreatePipe(&r, &w, NULL, 0));
    DWORD n = 0;
    char buf[32] = {0};
    CHECK("write", WriteFile(w, "hello", 5, &n, NULL) && n == 5);
    CHECK("read", ReadFile(r, buf, sizeof buf, &n, NULL) && n == 5 && !memcmp(buf, "hello", 5));
    CHECK("device type is a named pipe", GetFileType(r) == FILE_TYPE_PIPE);
    CloseHandle(w);
    SetLastError(0);
    CHECK("EOF after the writer closes", !ReadFile(r, buf, sizeof buf, &n, NULL) && GetLastError() == ERROR_BROKEN_PIPE);
    CloseHandle(r);

    /* A second pipe: the reader waits until a thread writes. */
    CHECK("CreatePipe again", CreatePipe(&r, &late_w, NULL, 0));
    HANDLE t = CreateThread(NULL, 0, write_later, NULL, 0, NULL);
    CHECK("blocked read woken", ReadFile(r, buf, sizeof buf, &n, NULL) && n == 4 && !memcmp(buf, "late", 4));
    WaitForSingleObject(t, INFINITE);
    CloseHandle(late_w);
    CloseHandle(r);

    /* A named duplex pipe, opened by name. */
    HANDLE srv = CreateNamedPipeA("\\\\.\\pipe\\gex-test", PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE, 1, 512, 512, 0, NULL);
    CHECK("CreateNamedPipe", srv != INVALID_HANDLE_VALUE);
    HANDLE cli = CreateFileA("\\\\.\\pipe\\gex-test", GENERIC_READ | GENERIC_WRITE, 0, NULL, OPEN_EXISTING, 0, NULL);
    CHECK("client open", cli != INVALID_HANDLE_VALUE);
    CHECK("client to server", WriteFile(cli, "ping", 4, &n, NULL) && ReadFile(srv, buf, 4, &n, NULL) && !memcmp(buf, "ping", 4));
    CHECK("server to client", WriteFile(srv, "pong", 4, &n, NULL) && ReadFile(cli, buf, 4, &n, NULL) && !memcmp(buf, "pong", 4));
    CloseHandle(cli);
    CloseHandle(srv);
    HANDLE gone = CreateFileA("\\\\.\\pipe\\gex-test", GENERIC_READ, 0, NULL, OPEN_EXISTING, 0, NULL);
    CHECK("name gone with its server", gone == INVALID_HANDLE_VALUE && GetLastError() == ERROR_FILE_NOT_FOUND);

    QSIEx q = (QSIEx)GetProcAddress(GetModuleHandleA("ntdll.dll"), "NtQuerySystemInformationEx");
    HANDLE self = GetCurrentProcess();
    DWORD arch[8] = {0};
    ULONG len = 0;
    NTSTATUS s = q ? q(181, &self, sizeof self, arch, sizeof arch, &len) : -1;
    CHECK("SystemSupportedProcessorArchitectures", s == 0 && (arch[0] & 0xffff) == 0x8664 && (arch[0] & (1 << 18)));
    printf("%d failures\n", failures);
    return failures != 0;
}
