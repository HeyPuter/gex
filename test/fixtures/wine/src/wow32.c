/* gex-nt-arch: x86
 * gex-nt-libs: -lopengl32 -lgdi32 */
/* WoW64 rows for a 32-bit program in a 64-bit Wine process (run.sh).
 * i686-w64-mingw32; one case per run, named by argv[1]:
 *
 *   hello     console output
 *   wow       IsWow64Process TRUE, IsWow64Process2 I386 on AMD64,
 *             GetSystemWow64Directory, FS:[0x18] is the TEB and FS:[0x30]
 *             the PEB below 4 GiB
 *   seh       RaiseException through a 32-bit handler chain (two frames,
 *             the inner one passes)
 *   av        a store to a no-access page, through a vectored handler, with
 *             the address in the record
 *   sehstack  a fault (ud2) on a nearly full 32-bit stack: nothing of the
 *             64-bit dispatch lands on it
 *   faultctx  the record and context a 32-bit handler gets for a fault (ud2)
 *             in 32-bit code: code, address, Eip, Esp, CS and the registers
 *   threads   CreateThread x4, TLS per thread, FS:[0x18] per thread, and
 *             GetThreadContext on a suspended thread (Eip in its code)
 *   cbnest N  a window proc that sends itself a message from inside a
 *             message, N deep (user callbacks nested through wow64)
 *   spawn64 EXE  CreateProcess of a 64-bit program, its exit code back
 *   glmap     WGL on a window: glClear read back with glReadPixels, then a
 *             buffer's glMapBuffer, glMapBufferRange (read) and, on GL 4.4,
 *             a persistent coherent glBufferStorage map: what 32-bit code
 *             writes through each map reads back with glGetBufferSubData
 *             (the 64-bit GL driver's mappings, as wow64 hands them to
 *             32-bit code)
 *   maskmov   maskmovdqu: the masked bytes only, to [edi]
 *   bcd       fbstp and fbld (packed BCD, as Delphi's float formatting
 *             uses them): 1234567.5 rounds to even, and back
 *   sysarch   GetSystemInfo INTEL and GetNativeSystemInfo AMD64, as a
 *             32-bit program sees a 64-bit system
 *   sysbench N   N calls of NtQueryPerformanceCounter, then of
 *             NtQueryVirtualMemory, system calls with no server request:
 *             wall time per call (a measurement, timed
 *             only on a bench host; it passes if the calls succeed)
 *
 * Each case prints "ok   CASE ..." lines and ends "wow32 CASE ok", or
 * prints "FAIL ..." and exits 1. */
#include <windows.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <GL/gl.h>

static int fails;
#define CHECK(c, ...) do { if (c) { printf("ok   "); printf(__VA_ARGS__); printf("\n"); } \
    else { printf("FAIL "); printf(__VA_ARGS__); printf("\n"); fails++; } fflush(stdout); } while (0)

static void *fs_ptr(unsigned off)
{
    void *p;
    __asm__ volatile("movl %%fs:(%1), %0" : "=r"(p) : "r"(off));
    return p;
}

static int case_hello(void)
{
    DWORD n;
    const char msg[] = "hello from a 32-bit program\n";
    BOOL w = WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), msg, sizeof(msg) - 1, &n, NULL);
    CHECK(w && n == sizeof(msg) - 1, "hello WriteFile %u bytes", (unsigned)n);
    return 0;
}

typedef BOOL (WINAPI *iswow2_t)(HANDLE, USHORT *, USHORT *);

static int case_wow(void)
{
    BOOL wow = FALSE;
    USHORT pm = 0, nm = 0;
    char dir[MAX_PATH];
    UINT len;
    iswow2_t p2 = (iswow2_t)GetProcAddress(GetModuleHandleA("kernel32.dll"), "IsWow64Process2");
    NT_TIB *tib = fs_ptr(0x18);

    CHECK(IsWow64Process(GetCurrentProcess(), &wow) && wow, "wow IsWow64Process %d", wow);
    if (p2) CHECK(p2(GetCurrentProcess(), &pm, &nm) && pm == IMAGE_FILE_MACHINE_I386 && nm == IMAGE_FILE_MACHINE_AMD64,
                  "wow IsWow64Process2 process %04x native %04x", pm, nm);
    else CHECK(0, "wow IsWow64Process2 exported");
    len = GetSystemWow64DirectoryA(dir, sizeof(dir));
    CHECK(len && !_stricmp(dir, "C:\\windows\\syswow64"), "wow GetSystemWow64Directory %s", len ? dir : "(none)");
    CHECK(tib == (NT_TIB *)NtCurrentTeb() && tib->Self == tib, "wow fs:[0x18] %p is the TEB", (void *)tib);
    CHECK(fs_ptr(0x30) != NULL, "wow fs:[0x30] PEB %p", fs_ptr(0x30));
    return 0;
}

static volatile int seh_inner_seen, seh_outer_seen;

static EXCEPTION_DISPOSITION __cdecl inner_handler(EXCEPTION_RECORD *rec, void *frame, CONTEXT *ctx, void *disp)
{
    if (rec->ExceptionFlags & (EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND)) return ExceptionContinueSearch;
    seh_inner_seen = 1;
    return ExceptionContinueSearch;
}

static EXCEPTION_DISPOSITION __cdecl outer_handler(EXCEPTION_RECORD *rec, void *frame, CONTEXT *ctx, void *disp)
{
    if (rec->ExceptionFlags & (EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND)) return ExceptionContinueSearch;
    if (rec->ExceptionCode == 0xe0000042 && seh_inner_seen)
    {
        seh_outer_seen = 1;
        return ExceptionContinueExecution; /* RaiseException's caller goes on */
    }
    return ExceptionContinueSearch;
}

struct seh_reg { struct seh_reg *prev; void *handler; };

static void push_frame(struct seh_reg *r, void *h)
{
    r->handler = h;
    __asm__ volatile("movl %%fs:0, %0" : "=r"(r->prev));
    __asm__ volatile("movl %0, %%fs:0" :: "r"(r) : "memory");
}

static void pop_frame(struct seh_reg *r)
{
    __asm__ volatile("movl %0, %%fs:0" :: "r"(r->prev) : "memory");
}

/* A no-access page to fault on: gex keeps the page at 0 accessible unless
 * its null-page guard (a debug switch) is on, so the rows fault here. */
static BYTE *noaccess_page(void)
{
    static BYTE *page;
    DWORD old;
    if (!page && (page = VirtualAlloc(NULL, 0x1000, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)))
        VirtualProtect(page, 0x1000, PAGE_NOACCESS, &old);
    return page;
}

static volatile int av_count;

static LONG WINAPI av_filter(EXCEPTION_POINTERS *ep)
{
    if (ep->ExceptionRecord->ExceptionCode == EXCEPTION_ACCESS_VIOLATION &&
        ep->ExceptionRecord->ExceptionInformation[1] == (ULONG_PTR)noaccess_page() + 0x10)
    {
        av_count++;
        ep->ContextRecord->Eip += 2; /* past the store (below) */
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    return EXCEPTION_CONTINUE_SEARCH;
}

static int case_seh(void)
{
    struct seh_reg outer, inner;
    void *old = SetUnhandledExceptionFilter(NULL);

    /* RaiseException through two 32-bit frames: the inner handler passes,
     * the outer continues execution */
    push_frame(&outer, outer_handler);
    push_frame(&inner, inner_handler);
    RaiseException(0xe0000042, 0, 0, NULL);
    pop_frame(&inner);
    pop_frame(&outer);
    CHECK(seh_inner_seen && seh_outer_seen, "seh RaiseException chain inner %d outer %d", seh_inner_seen, seh_outer_seen);
    SetUnhandledExceptionFilter(old);
    return 0;
}

static int case_av(void)
{
    /* an access violation in 32-bit code, through a vectored handler, the
     * faulting Eip and address in the 32-bit context: a 4-byte store to a
     * no-access page (mov %eax, (%edx): 89 02, 2 bytes), resumed past it */
    {
        PVOID h = AddVectoredExceptionHandler(1, av_filter);
        __asm__ volatile(".byte 0x89, 0x02" :: "d"(noaccess_page() + 0x10) : "memory");
        RemoveVectoredExceptionHandler(h);
    }
    CHECK(av_count == 1, "av access violation at page+0x10 seen %d", av_count);
    return 0;
}

/* faultctx: a ud2 at a known instruction with known registers.
 * The 64-bit side dispatches it below r14 and wow64 hands it to the 32-bit
 * side; the handler must see the fault as it happened in 32-bit code. */
static EXCEPTION_RECORD fc_rec;
static CONTEXT fc_ctx;
static DWORD fc_esp;
static volatile int fc_seen;
extern char fc_insn[];

static LONG WINAPI fc_filter(EXCEPTION_POINTERS *ep)
{
    if (ep->ExceptionRecord->ExceptionCode != EXCEPTION_ILLEGAL_INSTRUCTION ||
        ep->ExceptionRecord->ExceptionAddress != (void *)fc_insn)
        return EXCEPTION_CONTINUE_SEARCH;
    fc_rec = *ep->ExceptionRecord;
    fc_ctx = *ep->ContextRecord;
    fc_seen++;
    ep->ContextRecord->Eip += 2; /* past the ud2 (below) */
    return EXCEPTION_CONTINUE_EXECUTION;
}

static int case_faultctx(void)
{
    PVOID h = AddVectoredExceptionHandler(1, fc_filter);

    __asm__ volatile(
        "pushl %%ebx\n\t"
        "pushl %%esi\n\t"
        "pushl %%edi\n\t"
        "pushl %%ebp\n\t"
        "movl $0x11223344, %%ebx\n\t"
        "movl $0x55667788, %%esi\n\t"
        "movl $0x99aabbcc, %%edi\n\t"
        "movl $0x0ddba11f, %%ebp\n\t"
        "movl %%esp, %0\n\t"
        ".globl _fc_insn\n"
        "_fc_insn:\n\t"
        "ud2\n\t"
        "popl %%ebp\n\t"
        "popl %%edi\n\t"
        "popl %%esi\n\t"
        "popl %%ebx\n\t"
        : "=m"(fc_esp) :: "memory");
    RemoveVectoredExceptionHandler(h);
    CHECK(fc_seen == 1, "faultctx fault seen %d", fc_seen);
    CHECK(fc_rec.ExceptionCode == EXCEPTION_ILLEGAL_INSTRUCTION && fc_rec.ExceptionAddress == (void *)fc_insn,
          "faultctx record: code %08lx address %p (insn %p)", fc_rec.ExceptionCode, fc_rec.ExceptionAddress, fc_insn);
    CHECK(fc_ctx.Eip == (DWORD)fc_insn && fc_ctx.Esp == fc_esp && fc_ctx.SegCs == 0x23,
          "faultctx context: eip %08lx esp %08lx cs %04lx (want %08lx %08lx 0023)",
          fc_ctx.Eip, fc_ctx.Esp, fc_ctx.SegCs, (DWORD)fc_insn, fc_esp);
    CHECK(fc_ctx.Ebx == 0x11223344 && fc_ctx.Esi == 0x55667788 && fc_ctx.Edi == 0x99aabbcc && fc_ctx.Ebp == 0x0ddba11f,
          "faultctx registers: ebx %08lx esi %08lx edi %08lx ebp %08lx", fc_ctx.Ebx, fc_ctx.Esi, fc_ctx.Edi, fc_ctx.Ebp);
    return 0;
}

static int case_sysbench(int n)
{
    LONG (WINAPI *qpc)(LARGE_INTEGER *, LARGE_INTEGER *) =
        (void *)GetProcAddress(GetModuleHandleA("ntdll.dll"), "NtQueryPerformanceCounter");
    LARGE_INTEGER c, f, t0, t1;
    LONG st = 0;
    int i;

    if (!qpc) { CHECK(0, "sysbench NtQueryPerformanceCounter"); return 0; }
    QueryPerformanceFrequency(&f);
    QueryPerformanceCounter(&t0);
    for (i = 0; i < n; i++) st |= qpc(&c, NULL);
    QueryPerformanceCounter(&t1);
    CHECK(st == 0, "sysbench %d calls: %.1f ns per call (%.3f s)", n,
          (double)(t1.QuadPart - t0.QuadPart) * 1e9 / f.QuadPart / n,
          (double)(t1.QuadPart - t0.QuadPart) / f.QuadPart);
    /* and one that every Wine makes a system call: NtQueryPerformanceCounter
     * has a call-free path in a native ntdll */
    {
        LONG (WINAPI *qvm)(HANDLE, const void *, ULONG, void *, SIZE_T, SIZE_T *) =
            (void *)GetProcAddress(GetModuleHandleA("ntdll.dll"), "NtQueryVirtualMemory");
        MEMORY_BASIC_INFORMATION mbi;

        QueryPerformanceCounter(&t0);
        for (i = 0; i < n; i++) st |= qvm(GetCurrentProcess(), &mbi, 0 /* MemoryBasicInformation */, &mbi, sizeof(mbi), NULL);
        QueryPerformanceCounter(&t1);
        CHECK(st == 0, "sysbench %d NtQueryVirtualMemory calls: %.1f ns per call (%.3f s)", n,
              (double)(t1.QuadPart - t0.QuadPart) * 1e9 / f.QuadPart / n,
              (double)(t1.QuadPart - t0.QuadPart) / f.QuadPart);
    }
    return 0;
}

/* sehstack: a fault (ud2) in 32-bit code on a nearly full 32-bit
 * stack (8 KiB above a no-access page). The 64-bit side dispatches it on
 * the thread's 64-bit stack, so all that lands on the 32-bit stack is the
 * 32-bit dispatch itself: the 32-bit CONTEXT just below the faulting esp
 * and the handler's frames below that, nothing lower. */
static DWORD st_saved_esp, st_fault_esp, st_handler_esp, st_ctx_addr, st_ctx_esp;
static volatile int st_seen;
extern char st_insn[];

static LONG WINAPI st_filter(EXCEPTION_POINTERS *ep)
{
    if (ep->ExceptionRecord->ExceptionCode != EXCEPTION_ILLEGAL_INSTRUCTION ||
        ep->ExceptionRecord->ExceptionAddress != (void *)st_insn)
        return EXCEPTION_CONTINUE_SEARCH;
    __asm__ volatile("movl %%esp, %0" : "=m"(st_handler_esp));
    st_ctx_addr = (DWORD)ep->ContextRecord;
    st_ctx_esp = ep->ContextRecord->Esp;
    st_seen++;
    ep->ContextRecord->Eip += 2; /* past the ud2 */
    return EXCEPTION_CONTINUE_EXECUTION;
}

static int case_sehstack(void)
{
    const DWORD size = 0x10000, used_room = 0x2000;
    BYTE *lo = VirtualAlloc(NULL, size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE), *p;
    DWORD old, top, low_written = 0, saved_base, saved_limit;
    PVOID h;

    if (!lo) { CHECK(0, "sehstack VirtualAlloc"); return 0; }
    VirtualProtect(lo, 0x1000, PAGE_NOACCESS, &old);
    top = (DWORD)lo + 0x1000 + used_room;
    memset(lo + 0x1000, 0xa5, used_room);
    h = AddVectoredExceptionHandler(1, st_filter);
    saved_base = (DWORD)fs_ptr(4);
    saved_limit = (DWORD)fs_ptr(8);
    /* the TEB's 32-bit stack range is the small stack's for the fault */
    __asm__ volatile(
        "movl %%esp, %0\n\t"
        "movl %2, %%fs:4\n\t"
        "movl %3, %%fs:8\n\t"
        "movl %2, %%esp\n\t"
        "movl %%esp, %1\n\t"
        ".globl _st_insn\n"
        "_st_insn:\n\t"
        "ud2\n\t"
        "movl %0, %%esp\n\t"
        "movl %4, %%fs:4\n\t"
        "movl %5, %%fs:8\n\t"
        : "=m"(st_saved_esp), "=m"(st_fault_esp)
        : "r"(top), "r"((DWORD)lo + 0x1000), "r"(saved_base), "r"(saved_limit)
        : "memory");
    RemoveVectoredExceptionHandler(h);
    for (p = lo + 0x1000; p < (BYTE *)top; p++) if (*p != 0xa5) { low_written = (DWORD)p; break; }
    CHECK(st_seen == 1 && st_ctx_esp == st_fault_esp, "sehstack fault seen %d, context esp %08lx (fault esp %08lx)",
          st_seen, st_ctx_esp, st_fault_esp);
    CHECK(st_fault_esp - st_ctx_addr <= 0x800, "sehstack 32-bit CONTEXT at %08lx, %lu bytes below the faulting esp",
          st_ctx_addr, st_fault_esp - st_ctx_addr);
    CHECK(!low_written || low_written + 0x400 >= st_handler_esp,
          "sehstack lowest byte written %08lx, handler esp %08lx (nothing below the 32-bit dispatch)",
          low_written, st_handler_esp);
    VirtualFree(lo, 0, MEM_RELEASE);
    return 0;
}

static DWORD tls_index;
static volatile LONG thread_ok;
static volatile LONG spin_go;

static DWORD WINAPI worker(void *arg)
{
    int id = (int)(INT_PTR)arg;
    NT_TIB *tib = fs_ptr(0x18);

    TlsSetValue(tls_index, (void *)(INT_PTR)(id + 100));
    Sleep(10);
    if (TlsGetValue(tls_index) == (void *)(INT_PTR)(id + 100) && tib == (NT_TIB *)NtCurrentTeb() &&
        GetCurrentThreadId() == (DWORD)(ULONG_PTR)((void **)tib)[9])
        InterlockedIncrement(&thread_ok);
    return id;
}

static DWORD WINAPI spinner(void *arg)
{
    while (!spin_go) __asm__ volatile("pause");
    return 0;
}

static int case_threads(void)
{
    HANDLE h[4], s;
    CONTEXT ctx;
    DWORD code;
    int i, ok = 1;

    tls_index = TlsAlloc();
    for (i = 0; i < 4; i++) h[i] = CreateThread(NULL, 0, worker, (void *)(INT_PTR)i, 0, NULL);
    WaitForMultipleObjects(4, h, TRUE, 30000);
    for (i = 0; i < 4; i++)
    {
        GetExitCodeThread(h[i], &code);
        if (code != (DWORD)i) ok = 0;
        CloseHandle(h[i]);
    }
    CHECK(ok && thread_ok == 4, "threads 4 threads, TLS and TEB each: %ld of 4", thread_ok);

    s = CreateThread(NULL, 0, spinner, NULL, 0, NULL);
    Sleep(50);
    SuspendThread(s);
    memset(&ctx, 0, sizeof(ctx));
    ctx.ContextFlags = CONTEXT_CONTROL | CONTEXT_INTEGER;
    ok = GetThreadContext(s, &ctx);
    CHECK(ok && ctx.SegCs == 0x23 && ctx.Esp && ctx.Eip, "threads GetThreadContext of a suspended thread: cs %04lx eip %08lx esp %08lx",
          ctx.SegCs, ctx.Eip, ctx.Esp);
    spin_go = 1;
    ResumeThread(s);
    WaitForSingleObject(s, 30000);
    CloseHandle(s);
    return 0;
}

static int cb_max, cb_reached;

static LRESULT CALLBACK nest_proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    if (msg == WM_USER + 1)
    {
        int d = (int)wp;
        if (d > cb_reached) cb_reached = d;
        if (d < cb_max) return SendMessageA(hwnd, WM_USER + 1, d + 1, lp + 1) + 1;
        return 1;
    }
    return DefWindowProcA(hwnd, msg, wp, lp);
}

static int case_cbnest(int depth)
{
    WNDCLASSA wc = { 0 };
    HWND hwnd;
    LRESULT r;

    wc.lpfnWndProc = nest_proc;
    wc.hInstance = GetModuleHandleA(NULL);
    wc.lpszClassName = "wow32nest";
    CHECK(RegisterClassA(&wc) != 0, "cbnest RegisterClass");
    hwnd = CreateWindowA("wow32nest", "nest", 0, 0, 0, 10, 10, HWND_MESSAGE, NULL, wc.hInstance, NULL);
    CHECK(hwnd != NULL, "cbnest CreateWindow (message-only)");
    if (!hwnd) return 0;
    cb_max = depth;
    r = SendMessageA(hwnd, WM_USER + 1, 1, 0);
    CHECK(r == depth && cb_reached == depth, "cbnest depth %d: reached %d, result %ld", depth, cb_reached, (long)r);
    DestroyWindow(hwnd);
    return 0;
}

static int case_spawn64(const char *exe)
{
    STARTUPINFOA si = { sizeof(si) };
    PROCESS_INFORMATION pi;
    DWORD code = 0;
    BOOL wow = TRUE;
    char cmd[MAX_PATH + 16];

    snprintf(cmd, sizeof(cmd), "\"%s\" child", exe);
    if (!CreateProcessA(NULL, cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi))
    {
        CHECK(0, "spawn64 CreateProcess %s: error %lu", exe, GetLastError());
        return 0;
    }
    WaitForSingleObject(pi.hProcess, 120000);
    GetExitCodeProcess(pi.hProcess, &code);
    IsWow64Process(pi.hProcess, &wow);
    CHECK(code == 64 && !wow, "spawn64 64-bit child exit %lu, IsWow64Process(child) %d", code, wow);
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
    return 0;
}

/* glmap: GL entry points past 1.1, through wglGetProcAddress */
typedef ptrdiff_t GLsizeiptr_, GLintptr_;
#define GL_ARRAY_BUFFER_ 0x8892
#define GL_DYNAMIC_DRAW_ 0x88E8
#define GL_READ_ONLY_ 0x88B8
#define GL_WRITE_ONLY_ 0x88B9
#define GL_MAP_READ_BIT_ 0x0001
#define GL_MAP_WRITE_BIT_ 0x0002
#define GL_MAP_PERSISTENT_BIT_ 0x0040
#define GL_MAP_COHERENT_BIT_ 0x0080
static void (WINAPI *pglGenBuffers)(GLsizei, GLuint *);
static void (WINAPI *pglBindBuffer)(GLenum, GLuint);
static void (WINAPI *pglBufferData)(GLenum, GLsizeiptr_, const void *, GLenum);
static void (WINAPI *pglBufferStorage)(GLenum, GLsizeiptr_, const void *, GLbitfield);
static void *(WINAPI *pglMapBuffer)(GLenum, GLenum);
static void *(WINAPI *pglMapBufferRange)(GLenum, GLintptr_, GLsizeiptr_, GLbitfield);
static GLboolean (WINAPI *pglUnmapBuffer)(GLenum);
static void (WINAPI *pglGetBufferSubData)(GLenum, GLintptr_, GLsizeiptr_, void *);

static int glmap_check(const char *what, unsigned seed, int n)
{
    static unsigned back[1 << 18];
    int i, bad = -1;
    memset(back, 0, n * 4);
    pglGetBufferSubData(GL_ARRAY_BUFFER_, 0, n * 4, back);
    for (i = 0; i < n && bad < 0; i++) if (back[i] != seed * 2654435761u + i) bad = i;
    CHECK(bad < 0, "glmap %s: %d words written through the map read back%s", what, n,
          bad < 0 ? "" : " (a word differs)");
    if (bad >= 0) printf("  word %d: %08x, want %08x\n", bad, back[bad], seed * 2654435761u + bad);
    return bad < 0;
}

__attribute__((target("sse2"))) static int case_maskmov(void)
{
    static const unsigned char src[16] = { 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16 };
    static const unsigned char mask[16] = { 0x80, 0, 0xff, 0x7f, 0, 0x81, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x90 };
    unsigned char dst[24];
    int k, bad = 0;

    memset(dst, 0xee, sizeof(dst));
    __asm__ volatile("movdqu (%0), %%xmm0\n\tmovdqu (%1), %%xmm1\n\tmaskmovdqu %%xmm1, %%xmm0"
                     :: "r"(src), "r"(mask), "D"(dst + 4) : "xmm0", "xmm1", "memory");
    for (k = 0; k < 24; k++)
    {
        int i = k - 4;
        unsigned char want = i >= 0 && i < 16 && (mask[i] & 0x80) ? src[i] : 0xee;
        if (dst[k] != want) bad++;
    }
    CHECK(!bad, "maskmov maskmovdqu %d wrong bytes", bad);
    return 0;
}

static int case_bcd(void)
{
    static const unsigned char want[10] = { 0x68, 0x45, 0x23, 0x01, 0, 0, 0, 0, 0, 0x80 };
    unsigned char bcd[10];
    double in = -1234567.5, out = 0;

    __asm__ volatile("fldl %2\n\tfbstp %0\n\tfbld %0\n\tfstpl %1" : "=m"(bcd), "=m"(out) : "m"(in));
    CHECK(!memcmp(bcd, want, 10), "bcd fbstp %02x%02x%02x%02x..%02x", bcd[3], bcd[2], bcd[1], bcd[0], bcd[9]);
    CHECK(out == -1234568.0, "bcd fbld %.1f", out);
    return 0;
}

static int case_sysarch(void)
{
    SYSTEM_INFO si, ni;

    GetSystemInfo(&si);
    GetNativeSystemInfo(&ni);
    CHECK(si.wProcessorArchitecture == PROCESSOR_ARCHITECTURE_INTEL, "sysarch GetSystemInfo %u", si.wProcessorArchitecture);
    CHECK(ni.wProcessorArchitecture == PROCESSOR_ARCHITECTURE_AMD64, "sysarch GetNativeSystemInfo %u", ni.wProcessorArchitecture);
    return 0;
}

static int case_glmap(void)
{
    enum { N = 1 << 18 };
    PIXELFORMATDESCRIPTOR pfd = { sizeof pfd, 1, PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
                                  PFD_TYPE_RGBA, 32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 24 };
    WNDCLASSA wc = { 0 };
    unsigned char px[4] = { 0 };
    const char *ver;
    unsigned *p;
    GLuint b[3];
    HGLRC rc;
    HWND h;
    HDC dc;
    int fmt, i, major = 0, minor = 0;

    wc.lpfnWndProc = DefWindowProcA;
    wc.hInstance = GetModuleHandleA(NULL);
    wc.lpszClassName = "glmap";
    RegisterClassA(&wc);
    h = CreateWindowA("glmap", "glmap", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 0, 0, 160, 120, NULL, NULL, wc.hInstance, NULL);
    dc = h ? GetDC(h) : NULL;
    fmt = dc ? ChoosePixelFormat(dc, &pfd) : 0;
    CHECK(fmt && SetPixelFormat(dc, fmt, &pfd), "glmap pixel format %d", fmt);
    rc = fmt ? wglCreateContext(dc) : NULL;
    CHECK(rc && wglMakeCurrent(dc, rc), "glmap WGL context current");
    if (fails) return 1;
    ver = (const char *)glGetString(GL_VERSION);
    printf("     GL %s, %s\n", ver ? ver : "?", (const char *)glGetString(GL_RENDERER));
    if (ver) sscanf(ver, "%d.%d", &major, &minor);

    glClearColor(0.25f, 0.5f, 0.75f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    CHECK(abs(px[0] - 64) <= 1 && abs(px[1] - 128) <= 1 && abs(px[2] - 191) <= 1,
          "glmap glClear reads back %d,%d,%d (want 64,128,191)", px[0], px[1], px[2]);

#define GLP(n) (*(PROC *)&p##n = wglGetProcAddress(#n))
    GLP(glGenBuffers); GLP(glBindBuffer); GLP(glBufferData); GLP(glMapBuffer);
    GLP(glMapBufferRange); GLP(glUnmapBuffer); GLP(glGetBufferSubData); GLP(glBufferStorage);
    CHECK(pglGenBuffers && pglBindBuffer && pglBufferData && pglMapBuffer && pglMapBufferRange && pglUnmapBuffer
          && pglGetBufferSubData, "glmap buffer entry points");
    if (fails) return 1;
    pglGenBuffers(3, b);

    /* glMapBuffer, write only */
    pglBindBuffer(GL_ARRAY_BUFFER_, b[0]);
    pglBufferData(GL_ARRAY_BUFFER_, N * 4, NULL, GL_DYNAMIC_DRAW_);
    p = pglMapBuffer(GL_ARRAY_BUFFER_, GL_WRITE_ONLY_);
    CHECK(p != NULL, "glmap glMapBuffer %p (error %04x)", (void *)p, glGetError());
    if (p) {
        for (i = 0; i < N; i++) p[i] = 1 * 2654435761u + i;
        CHECK(pglUnmapBuffer(GL_ARRAY_BUFFER_), "glmap glUnmapBuffer");
        glmap_check("glMapBuffer", 1, N);
    }

    /* glMapBufferRange, read: what glBufferData put there */
    {
        static unsigned src[N];
        for (i = 0; i < N; i++) src[i] = 2 * 2654435761u + i;
        pglBindBuffer(GL_ARRAY_BUFFER_, b[1]);
        pglBufferData(GL_ARRAY_BUFFER_, N * 4, src, GL_DYNAMIC_DRAW_);
        p = pglMapBufferRange(GL_ARRAY_BUFFER_, 4096, N * 4 - 4096, GL_MAP_READ_BIT_);
        CHECK(p != NULL, "glmap glMapBufferRange %p (error %04x)", (void *)p, glGetError());
        if (p) {
            int bad = -1;
            for (i = 0; i < N - 1024 && bad < 0; i++) if (p[i] != src[i + 1024]) bad = i;
            CHECK(bad < 0, "glmap glMapBufferRange (read, offset 4096): %d words as uploaded", N - 1024);
            pglUnmapBuffer(GL_ARRAY_BUFFER_);
        }
    }

    /* glBufferStorage, persistent and coherent: written while mapped */
    if (pglBufferStorage && (major > 4 || (major == 4 && minor >= 4))) {
        GLbitfield fl = GL_MAP_WRITE_BIT_ | GL_MAP_PERSISTENT_BIT_ | GL_MAP_COHERENT_BIT_;
        pglBindBuffer(GL_ARRAY_BUFFER_, b[2]);
        pglBufferStorage(GL_ARRAY_BUFFER_, N * 4, NULL, fl);
        p = pglMapBufferRange(GL_ARRAY_BUFFER_, 0, N * 4, fl);
        CHECK(p != NULL, "glmap persistent glMapBufferRange %p (error %04x)", (void *)p, glGetError());
        if (p) {
            for (i = 0; i < N; i++) p[i] = 3 * 2654435761u + i;
            glFinish();
            glmap_check("persistent coherent map", 3, N);
            pglUnmapBuffer(GL_ARRAY_BUFFER_);
        }
    } else printf("     glmap: no GL 4.4 glBufferStorage, persistent map not tried\n");

    wglMakeCurrent(NULL, NULL);
    wglDeleteContext(rc);
    ReleaseDC(h, dc);
    DestroyWindow(h);
    return 0;
}

int main(int argc, char **argv)
{
    const char *c = argc > 1 ? argv[1] : "hello";

    setvbuf(stdout, NULL, _IONBF, 0);
    if (!strcmp(c, "hello")) case_hello();
    else if (!strcmp(c, "wow")) case_wow();
    else if (!strcmp(c, "seh")) case_seh();
    else if (!strcmp(c, "sehstack")) case_sehstack();
    else if (!strcmp(c, "faultctx")) case_faultctx();
    else if (!strcmp(c, "av")) case_av();
    else if (!strcmp(c, "sysbench")) case_sysbench(argc > 2 ? atoi(argv[2]) : 100000);
    else if (!strcmp(c, "threads")) case_threads();
    else if (!strcmp(c, "cbnest")) case_cbnest(argc > 2 ? atoi(argv[2]) : 8);
    else if (!strcmp(c, "spawn64") && argc > 2) case_spawn64(argv[2]);
    else if (!strcmp(c, "glmap")) case_glmap();
    else if (!strcmp(c, "sysarch")) case_sysarch();
    else if (!strcmp(c, "bcd")) case_bcd();
    else if (!strcmp(c, "maskmov")) case_maskmov();
    else if (!strcmp(c, "child")) return 32;
    else { printf("FAIL unknown case %s\n", c); return 1; }
    if (fails) return 1;
    printf("wow32 %s ok\n", c);
    return 0;
}
