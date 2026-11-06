/* test/wow64/consolecoord: the console functions that take a COORD by
 * value (one word, which the .spec spells as an int and wasm32 takes by
 * reference) in a console of the program's own: a child it starts with
 * CREATE_NEW_CONSOLE (Wine's conhost, headless here) resizes a screen
 * buffer, moves its cursor, fills three cells and reads them back, and
 * writes what it saw to coord.out; then the same through unicows, which
 * exports kernel32's. Prints "consolecoord ok". */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int child(void)
{
    HANDLE h = CreateConsoleScreenBuffer(GENERIC_READ | GENERIC_WRITE, 0, NULL, CONSOLE_TEXTMODE_BUFFER, NULL);
    CONSOLE_SCREEN_BUFFER_INFO info;
    COORD at = { 5, 3 }, size = { 90, 30 };
    WCHAR buf[8] = { 0 };
    DWORD n = 0;
    FILE *f = fopen("coord.out", "w");
    if (!f) return 1;
    fprintf(f, "buffer %d\n", h != INVALID_HANDLE_VALUE);
    fprintf(f, "size %d\n", SetConsoleScreenBufferSize(h, size));
    fprintf(f, "cursor %d\n", SetConsoleCursorPosition(h, at));
    if (GetConsoleScreenBufferInfo(h, &info))
        fprintf(f, "at %d,%d size %d,%d\n", info.dwCursorPosition.X, info.dwCursorPosition.Y, info.dwSize.X, info.dwSize.Y);
    fprintf(f, "fill %d\n", FillConsoleOutputCharacterW(h, L'x', 3, at, &n));
    at.X = 4;
    fprintf(f, "read %d [%ls]\n", ReadConsoleOutputCharacterW(h, buf, 5, at, &n), buf);
    /* the same through unicows, which exports kernel32's console functions */
    {
        HMODULE u = LoadLibraryA("unicows.dll");
        BOOL (WINAPI *fill)(HANDLE, WCHAR, DWORD, COORD, DWORD *) = (void *)GetProcAddress(u, "FillConsoleOutputCharacterW");
        BOOL (WINAPI *read)(HANDLE, WCHAR *, DWORD, COORD, DWORD *) = (void *)GetProcAddress(u, "ReadConsoleOutputCharacterW");
        COORD to = { 7, 4 };
        memset(buf, 0, sizeof(buf));
        if (fill && read) {
            fill(h, L'y', 2, to, &n);
            to.X = 6;
            read(h, buf, 4, to, &n);
            fprintf(f, "unicows [%ls]\n", buf);
        }
        else fprintf(f, "unicows missing\n");
    }
    return fclose(f) != 0;
}

int main(int argc, char **argv)
{
    static const char want[] = "buffer 1\nsize 1\ncursor 1\nat 5,3 size 90,30\nfill 1\nread 1 [ xxx ]\nunicows [ yy ]\n";
    STARTUPINFOA si = { sizeof(si) };
    PROCESS_INFORMATION pi;
    char cmd[MAX_PATH + 16], got[512] = "";
    FILE *f;
    size_t len;

    if (argc > 1 && !strcmp(argv[1], "child")) return child();
    setvbuf(stdout, NULL, _IONBF, 0);
    DeleteFileA("coord.out");
    snprintf(cmd, sizeof(cmd), "\"%s\" child", argv[0]);
    if (!CreateProcessA(NULL, cmd, NULL, NULL, FALSE, CREATE_NEW_CONSOLE, NULL, NULL, &si, &pi)) {
        printf("consolecoord: CreateProcess failed (%lu)\n", GetLastError());
        return 1;
    }
    WaitForSingleObject(pi.hProcess, 120000);
    if (!(f = fopen("coord.out", "r"))) {
        printf("consolecoord: the child wrote nothing\n");
        return 1;
    }
    len = fread(got, 1, sizeof(got) - 1, f);
    got[len] = 0;
    fclose(f);
    if (strcmp(got, want)) {
        printf("consolecoord: the child saw\n%swant\n%s", got, want);
        return 1;
    }
    printf("consolecoord ok\n");
    return 0;
}
