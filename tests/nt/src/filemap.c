/* File mappings: a read-only view of a file, and a writable one whose
 * changes reach the file through FlushViewOfFile and UnmapViewOfFile,
 * including a mapping that extends the file.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

static DWORD read_all(const char *path, char *buf, DWORD max) {
    HANDLE f = CreateFileA(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, 0, OPEN_EXISTING, 0, 0);
    DWORD n = 0;
    if (f == INVALID_HANDLE_VALUE) return 0;
    ReadFile(f, buf, max, &n, 0);
    CloseHandle(f);
    return n;
}

int main(void) {
    char tmp[MAX_PATH], path[MAX_PATH], buf[64];
    GetTempPathA(sizeof(tmp), tmp);
    snprintf(path, sizeof(path), "%sgex-filemap-%lu.bin", tmp, GetCurrentProcessId());

    HANDLE f = CreateFileA(path, GENERIC_READ | GENERIC_WRITE, 0, 0, CREATE_ALWAYS, 0, 0);
    DWORD n;
    WriteFile(f, "hello mapping", 13, &n, 0);
    HANDLE ro = CreateFileMappingA(f, 0, PAGE_READONLY, 0, 0, 0);
    const char *r = MapViewOfFile(ro, FILE_MAP_READ, 0, 0, 0);
    CHECK("a read-only view shows the file", r && !memcmp(r, "hello mapping", 13));
    UnmapViewOfFile(r);
    CloseHandle(ro);

    HANDLE rw = CreateFileMappingA(f, 0, PAGE_READWRITE, 0, 32, 0);
    CHECK("a writable mapping larger than the file extends it", rw && GetFileSize(f, 0) == 32);
    char *w = MapViewOfFile(rw, FILE_MAP_WRITE, 0, 0, 0);
    CHECK("a writable view", w != 0);
    memcpy(w, "HELLO", 5);
    w[31] = '!';
    CHECK("FlushViewOfFile", FlushViewOfFile(w, 0));
    UnmapViewOfFile(w);
    w = MapViewOfFile(rw, FILE_MAP_WRITE, 0, 0, 0);
    CHECK("the section maps writable again after an unmap", w && !memcmp(w, "HELLO", 5));
    UnmapViewOfFile(w);
    CloseHandle(rw);
    CloseHandle(f);
    memset(buf, 0, sizeof(buf));
    n = read_all(path, buf, sizeof(buf));
    CHECK("the writes reached the file", n == 32 && !memcmp(buf, "HELLO mapping", 13) && buf[31] == '!');
    DeleteFileA(path);
    printf("%d failures\n", failures);
    return failures;
}
