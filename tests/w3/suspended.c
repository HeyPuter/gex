/* A process created suspended runs nothing until it is resumed
 * (suspended.sh). The parent creates itself again with CREATE_SUSPENDED,
 * waits, and checks that the child has not made its marker file, which the
 * child does first thing in main; then it resumes the child and the marker
 * appears. Meanwhile a second thread of the parent keeps an anonymous pipe
 * busy, so the parent's own server traffic and pipe wakes run beside the
 * child's wait (Wine's wait_suspend: a server_select that returns only once
 * the thread is resumed).
 *
 *   suspended.exe [ROUNDS [WAIT_MS]]   prints "suspended ok ROUNDS"
 *   suspended.exe child MARKER         (the child) */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static volatile LONG stop;
static volatile LONG pumped;

static DWORD WINAPI pump(LPVOID arg) {
    HANDLE r, w;
    char buf[4096];
    (void)arg;
    if (!CreatePipe(&r, &w, NULL, 0)) return 1;
    memset(buf, 'p', sizeof buf);
    while (!stop) {
        DWORD n = 0;
        if (!WriteFile(w, buf, sizeof buf, &n, NULL) || !ReadFile(r, buf, n, &n, NULL)) break;
        InterlockedIncrement(&pumped);
    }
    CloseHandle(r);
    CloseHandle(w);
    return 0;
}

static int child(const char *marker) {
    HANDLE f = CreateFileA(marker, GENERIC_WRITE, 0, NULL, CREATE_NEW, 0, NULL);
    if (f == INVALID_HANDLE_VALUE) return 2;
    CloseHandle(f);
    return 7;
}

static BOOL exists(const char *p) {
    return GetFileAttributesA(p) != INVALID_FILE_ATTRIBUTES;
}

int main(int argc, char **argv) {
    char self[MAX_PATH], cmd[2 * MAX_PATH + 64], marker[MAX_PATH];
    int rounds = argc > 1 ? atoi(argv[1]) : 50, wait = argc > 2 ? atoi(argv[2]) : 2000;
    int early = 0, bad = 0;
    HANDLE th;
    if (argc > 2 && !strcmp(argv[1], "child")) return child(argv[2]);
    if (rounds <= 0) rounds = 50;
    GetModuleFileNameA(NULL, self, sizeof self);
    th = CreateThread(NULL, 0, pump, NULL, 0, NULL);
    for (int i = 0; i < rounds; i++) {
        STARTUPINFOA si = { sizeof si };
        PROCESS_INFORMATION pi;
        DWORD code = 0;
        snprintf(marker, sizeof marker, "C:\\suspended-%d.mark", i);
        DeleteFileA(marker);
        snprintf(cmd, sizeof cmd, "\"%s\" child %s", self, marker);
        if (!CreateProcessA(self, cmd, NULL, NULL, FALSE, CREATE_SUSPENDED, NULL, NULL, &si, &pi)) {
            printf("round %d: CreateProcess failed (%lu)\n", i, GetLastError());
            bad++;
            continue;
        }
        Sleep(wait);
        if (exists(marker)) {
            printf("round %d: the child ran before it was resumed\n", i);
            early++;
        }
        if (ResumeThread(pi.hThread) != 1) {
            printf("round %d: suspend count was not 1\n", i);
            bad++;
        }
        if (WaitForSingleObject(pi.hProcess, 60000) != WAIT_OBJECT_0) {
            printf("round %d: the child did not end\n", i);
            bad++;
            TerminateProcess(pi.hProcess, 1);
        } else if (!GetExitCodeProcess(pi.hProcess, &code) || code != 7 || !exists(marker)) {
            printf("round %d: the child ended with %lu, marker %s\n", i, code, exists(marker) ? "made" : "missing");
            bad++;
        }
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        DeleteFileA(marker);
    }
    stop = 1;
    WaitForSingleObject(th, INFINITE);
    if (early || bad || !pumped) {
        printf("suspended FAIL: %d early, %d other, %ld pipe rounds\n", early, bad, (long)pumped);
        return 1;
    }
    printf("suspended ok %d (%ld pipe rounds)\n", rounds, (long)pumped);
    fflush(stdout);
    return 0;
}
