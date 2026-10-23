/* What an exception nothing handles does: run as "unhandled.exe", each case
 * in a child ("unhandled.exe N"), whose exit code the parent prints as
 * "case N exit CODE", then "unhandled done". The code is the exception's,
 * as on Windows: the process ends with it whichever thread raised it, after
 * the program's top-level filter, which may also go on.
 *
 *  1  ud2, no filter                      c000001d
 *  2  ud2, the filter says execute        c000001d
 *  3  ud2, the filter resumes elsewhere   00000055 (its ExitProcess)
 *  4  ud2 on a second thread              c000001d
 *  5  RaiseException                      e0001234
 *  6  ud2 in a qsort comparator           c000001d (native frames between)
 *  7  6 on a second thread                c000001d
 *  8  int3                                80000003
 *  9  divide by zero                      c0000094
 * 10  a read at the old KUSER page        c0000005
 * 11  ud2 with the SEH chain emptied      c000001d (no handler at all)
 * 12  the filter itself faults            c000001d (nested)
 * 13  an SEH handler that always faults   c000001d (it ends)
 * 14  ud2 under DEEP nested qsorts        c000001d
 *
 * "unhandled.exe -N" runs cases 1 to N only.
 */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

#define NCASES 14
#define DEEP 400

static int which, filt_calls, depth_left;
extern void resume_here(void);

static void fault(void) { __asm__ volatile("ud2"); }

static void div0(void)
{
    __asm__ volatile("movl $1,%%eax\n\txorl %%edx,%%edx\n\txorl %%ecx,%%ecx\n\tidivl %%ecx" ::: "eax", "ecx", "edx");
}

static LONG WINAPI filt(EXCEPTION_POINTERS *ep)
{
    filt_calls++;
    printf("c%d filter %08lx\n", which, ep->ExceptionRecord->ExceptionCode);
    fflush(stdout);
    if (which == 3) {
        ep->ContextRecord->Eip = (DWORD)resume_here;
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    if (which == 12 && filt_calls == 1) fault();
    return EXCEPTION_EXECUTE_HANDLER;
}

__asm__(".globl _resume_here\n_resume_here:\n\tcall _after_resume\n1:\tjmp 1b\n");
void after_resume(void)
{
    printf("c%d resumed\n", which);
    fflush(stdout);
    ExitProcess(0x55);
}

static int cmp(const void *a, const void *b)
{
    if (*(const int *)a == 3) fault();
    return *(const int *)a - *(const int *)b;
}

static void sortfault(void)
{
    int v[] = {5, 3, 9, 1};
    qsort(v, 4, sizeof v[0], cmp);
}

static int dcmp(const void *a, const void *b);

static void deep(void)
{
    int v[] = {2, 1};
    qsort(v, 2, sizeof v[0], dcmp);
}

static int dcmp(const void *a, const void *b)
{
    if (depth_left-- > 0) deep();
    else fault();
    return *(const int *)a - *(const int *)b;
}

static EXCEPTION_DISPOSITION __cdecl bad_handler(EXCEPTION_RECORD *r, void *f, CONTEXT *c, void *d)
{
    fault();
    return ExceptionContinueSearch;
}

static DWORD WINAPI th(void *p)
{
    if (p) sortfault();
    else fault();
    return 7;
}

int main(int argc, char **argv)
{
    if (argc < 2 || argv[1][0] == '-') {
        int last = argc < 2 ? NCASES : atoi(argv[1] + 1);
        char self[MAX_PATH], cmd[MAX_PATH + 16];
        GetModuleFileNameA(NULL, self, MAX_PATH);
        for (int i = 1; i <= last; i++) {
            STARTUPINFOA si = {sizeof si};
            PROCESS_INFORMATION pi;
            DWORD code = 0xdead, w;
            snprintf(cmd, sizeof cmd, "\"%s\" %d", self, i);
            if (!CreateProcessA(self, cmd, NULL, NULL, TRUE, 0, NULL, NULL, &si, &pi)) {
                printf("case %d spawn failed %lu\n", i, GetLastError());
                continue;
            }
            w = WaitForSingleObject(pi.hProcess, 240000);
            if (w == WAIT_TIMEOUT) TerminateProcess(pi.hProcess, 1);
            GetExitCodeProcess(pi.hProcess, &code);
            printf("case %d exit %08lx%s\n", i, code, w == WAIT_TIMEOUT ? " (timeout)" : "");
            fflush(stdout);
            CloseHandle(pi.hProcess);
            CloseHandle(pi.hThread);
        }
        printf("unhandled done\n");
        return 0;
    }
    which = atoi(argv[1]);
    SetErrorMode(SEM_NOGPFAULTERRORBOX);
    if (which != 1 && which != 8) SetUnhandledExceptionFilter(filt);
    switch (which) {
    case 1: case 2: case 3: case 12: fault(); break;
    case 4: case 7: {
        HANDLE h = CreateThread(NULL, 0, th, (void *)(INT_PTR)(which == 7), 0, NULL);
        WaitForSingleObject(h, INFINITE);
        break;
    }
    case 5: RaiseException(0xe0001234, 0, 0, NULL); break;
    case 6: sortfault(); break;
    case 8: __asm__ volatile("int3"); break;
    case 9: div0(); break;
    case 10: printf("c10 read %d\n", *(volatile int *)0xfffff000); break;
    case 11: __asm__ volatile("movl $-1, %%fs:0\n\tud2" ::: "memory"); break;
    case 13:
        __asm__ volatile("pushl %0\n\tpushl %%fs:0\n\tmovl %%esp, %%fs:0\n\tud2" :: "r"(bad_handler) : "memory");
        break;
    case 14: depth_left = DEEP; deep(); break;
    }
    printf("c%d fell through\n", which);
    fflush(stdout);
    return 0x66;
}
