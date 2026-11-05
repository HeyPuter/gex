/* rewind() from ucrtbase on 32-bit Wine (test/wow64/rewind). ucrtbase's i386 rewind
 * export is rewind_preserve_stack, which the wasm32 build lacked: the call
 * trapped (a game's x86 run). The FILE must be ucrtbase's own, so every
 * call goes through ucrtbase's exports. Writes a temporary file, reads to EOF,
 * rewinds, reads again. Prints "rewind ok". */
#include <windows.h>
#include <stdio.h>
#include <string.h>

typedef void *(__cdecl *tmpfile_fn)(void);
typedef int (__cdecl *fputs_fn)(const char *, void *);
typedef char *(__cdecl *fgets_fn)(char *, int, void *);
typedef int (__cdecl *file_fn)(void *);
typedef void (__cdecl *rewind_fn)(void *);

int main(void)
{
    HMODULE u = LoadLibraryA("ucrtbase.dll");
    if (!u) { printf("rewind: no ucrtbase\n"); return 1; }
    tmpfile_fn tmp = (tmpfile_fn)GetProcAddress(u, "tmpfile");
    fputs_fn put = (fputs_fn)GetProcAddress(u, "fputs");
    fgets_fn get = (fgets_fn)GetProcAddress(u, "fgets");
    file_fn getc_ = (file_fn)GetProcAddress(u, "fgetc"), eof = (file_fn)GetProcAddress(u, "feof");
    file_fn close = (file_fn)GetProcAddress(u, "fclose");
    rewind_fn rew = (rewind_fn)GetProcAddress(u, "rewind");
    if (!tmp || !put || !get || !getc_ || !eof || !close || !rew) { printf("rewind: missing export\n"); return 1; }
    char buf[16] = {0};
    void *f = tmp();
    if (!f) { printf("rewind: no tmpfile\n"); return 1; }
    put("gex-rewind", f);
    rew(f);
    if (!get(buf, sizeof buf, f) || strcmp(buf, "gex-rewind")) { printf("rewind: first read got '%s'\n", buf); return 1; }
    if (getc_(f) != EOF || !eof(f)) { printf("rewind: no EOF\n"); return 1; }
    rew(f);
    if (eof(f)) { printf("rewind: EOF still set after rewind\n"); return 1; }
    memset(buf, 0, sizeof buf);
    if (!get(buf, sizeof buf, f) || strcmp(buf, "gex-rewind")) { printf("rewind: second read got '%s'\n", buf); return 1; }
    close(f);
    printf("rewind ok\n");
    return 0;
}
