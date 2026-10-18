/* A user-mode callback made from a Unix call, after the PE side has pushed
 * frames since its last system call (tests/w3/usercb.sh).
 *
 * On wasm32 a Unix call (__wine_unix_call) goes straight to the Unix
 * library: no system-call switch, so the thread's syscall frame still holds
 * the stack pointer of the last real system call. A callback the Unix side
 * makes from there (KeUserDispatchCallback: opengl32's GL debug messages,
 * which wined3d's command-stream thread gets) put its stack layout below that
 * old stack pointer -- on top of every frame the program had pushed since --
 * and the program came back to garbage (Momodora, Hades x86 in the page).
 *
 * The program: a GL context with a debug callback, one system call, then a
 * recursion of frames holding canaries, and at the bottom GL calls that make
 * opengl32's Unix side call back: glDebugMessageInsert (delivered inside the
 * call) and a GL error (delivered by the guest GL library at the next
 * submit that waits). Every canary must survive and the callback must run.
 * One "ok"/"FAIL" line a step, "usercb ok" at the end. */
#include <windows.h>
#include <GL/gl.h>
#include <stdio.h>
#include <string.h>

#define WGL_CONTEXT_MAJOR_VERSION_ARB 0x2091
#define WGL_CONTEXT_MINOR_VERSION_ARB 0x2092
#define WGL_CONTEXT_FLAGS_ARB 0x2094
#define WGL_CONTEXT_PROFILE_MASK_ARB 0x9126
#define WGL_CONTEXT_CORE_PROFILE_BIT_ARB 0x0001
#define WGL_CONTEXT_DEBUG_BIT_ARB 0x0001
#define WGL_CONTEXT_FORWARD_COMPATIBLE_BIT_ARB 0x0002

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok  " : "FAIL", name); fflush(stdout); if (!ok_) failures++; } while (0)

typedef HGLRC (WINAPI *create_t)(HDC, HGLRC, const int *);
typedef void (APIENTRY *debugproc_t)(GLenum, GLenum, GLuint, GLenum, GLsizei, const char *, const void *);
typedef void (APIENTRY *callback_t)(debugproc_t, const void *);
typedef void (APIENTRY *insert_t)(GLenum, GLenum, GLuint, GLenum, GLsizei, const char *);
static callback_t p_callback;

static volatile LONG calls;
/* The nesting case's state (debug_proc). */
#define NEST_MAX 100000
static int nest_on, nest_depth;
static char *nest_low; /* the deepest callback's padding */
static insert_t p_insert;
static const void *seen_user;
static char first[128];

static void APIENTRY debug_proc(GLenum source, GLenum type, GLuint id, GLenum severity, GLsizei length,
                                const char *message, const void *user) {
    (void)source; (void)type; (void)id; (void)severity;
    /* a frame of the callback's own, as wined3d's debug callback has */
    char line[256];
    snprintf(line, sizeof(line), "%.*s", length > 0 && length < 200 ? (int)length : 200, message ? message : "");
    if (!first[0]) memcpy(first, line, sizeof(first) - 1);
    seen_user = user;
    InterlockedIncrement(&calls);
    /* the nesting case: each message inserts the next from inside its
     * callback, until the Unix side has no room for another */
    if (nest_on && id == 9 && nest_depth < NEST_MAX) {
        /* 4 KiB a level: the chain runs out of stack in a few levels */
        volatile char pad[4096];
        pad[0] = pad[sizeof(pad) - 1] = 1;
        if (!nest_low || (char *)pad < nest_low) nest_low = (char *)pad;
        nest_depth++;
        p_insert(0x824A, 0x8251, 9, 0x9148, -1, "usercb: nested");
    }
}

#define DEPTH 24
#define WORDS 64
static int broken_level = -1;

/* DEPTH frames of WORDS canaries each (6 KiB of stack); at the bottom the
 * calls that call back, then every level checks its own canaries. */
static int __attribute__((noinline)) dive(int level, int mode) {
    volatile unsigned canary[WORDS];
    for (int i = 0; i < WORDS; i++) canary[i] = 0xc0de0000u + (unsigned)level * 256u + (unsigned)i;
    int r = 1;
    if (level < DEPTH) {
        r = dive(level + 1, mode);
    } else if (mode == 0) {
        p_insert(0x824A /* SOURCE_APPLICATION */, 0x8251 /* TYPE_OTHER */, 7, 0x9148 /* SEVERITY_LOW */, -1,
                 "usercb: a message inserted by the program");
    } else {
        unsigned char px[4];
        glViewport(0, 0, -1, -1);  /* GL_INVALID_VALUE, raised by the host */
        glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);  /* a submit that waits */
        glFinish();
    }
    for (int i = 0; i < WORDS; i++)
        if (canary[i] != 0xc0de0000u + (unsigned)level * 256u + (unsigned)i) {
            if (broken_level < 0) broken_level = level;
            return 0;
        }
    return r;
}

struct run { HDC dc; HGLRC gl; const int *tag; const char *who; };

static DWORD WINAPI run_modes(void *arg) {
    struct run *r = arg;
    char what[256];
    static const char *const names[] = {"glDebugMessageInsert", "a GL error at the next waiting submit"};
    wglMakeCurrent(r->dc, r->gl);
    for (int mode = 0; mode < 2; mode++) {
        LONG before = calls;
        first[0] = 0;
        broken_level = -1;
        seen_user = NULL;
        Sleep(0);  /* the thread's last system call before the dive */
        int intact = dive(0, mode);
        GLenum e = glGetError();
        snprintf(what, sizeof(what), "%s, %s: called back %ld time(s), user %s, \"%s\"", r->who, names[mode],
                 (long)(calls - before), seen_user == r->tag ? "kept" : "lost", first);
        CHECK(what, calls > before && seen_user == r->tag);
        snprintf(what, sizeof(what), "%s, %s: every frame's canaries intact (first broken level %d of %d)",
                 r->who, names[mode], broken_level, DEPTH);
        CHECK(what, intact);
        if (mode == 1) {
            snprintf(what, sizeof(what), "%s, %s: glGetError still says GL_INVALID_VALUE (%#x)", r->who, names[mode], e);
            CHECK(what, e == 0x0501);
        }
    }
    wglMakeCurrent(NULL, NULL);
    return 0;
}

static __attribute__((noinline)) void nest_start(void) {
    nest_on = 1;
    nest_depth = 0;
    p_insert(0x824A, 0x8251, 9, 0x9148, -1, "usercb: nested");
    nest_on = 0;
}

/* The chain starts 48 KiB above the end of the thread's stack (the rest is
 * taken first), so it runs out within a few levels, before gex's call-out
 * depth limit (32 call-outs, two a level) would end it. */
static DWORD WINAPI run_nest(void *arg) {
    struct run *r = arg;
    MEMORY_BASIC_INFORMATION mi = {0};
    volatile char *taken;
    VirtualQuery(&mi, &mi, sizeof(mi));
    taken = __builtin_alloca((char *)&mi - (char *)mi.AllocationBase - (48 << 10));
    taken[0] = 1;
    wglMakeCurrent(r->dc, r->gl);
    nest_start();
    wglMakeCurrent(NULL, NULL);
    return 0;
}

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM wp, LPARAM lp) { return DefWindowProcW(w, m, wp, lp); }

int main(void) {
    WNDCLASSW wc = {0};
    wc.style = CS_OWNDC;
    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleW(0);
    wc.lpszClassName = L"gex_usercb";
    RegisterClassW(&wc);
    HWND w = CreateWindowW(L"gex_usercb", L"usercb", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 0, 0, 64, 64, 0, 0, wc.hInstance, 0);
    HDC dc = GetDC(w);
    PIXELFORMATDESCRIPTOR pfd = {sizeof(pfd), 1, PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER, PFD_TYPE_RGBA, 32};
    SetPixelFormat(dc, ChoosePixelFormat(dc, &pfd), &pfd);
    HGLRC legacy = wglCreateContext(dc);
    wglMakeCurrent(dc, legacy);
    create_t create = (create_t)wglGetProcAddress("wglCreateContextAttribsARB");
    CHECK("wglCreateContextAttribsARB", create != NULL);
    if (!create) return 1;
    /* as wined3d asks while its d3d ERR channel is on */
    const int attribs[] = {WGL_CONTEXT_MAJOR_VERSION_ARB, 3, WGL_CONTEXT_MINOR_VERSION_ARB, 2,
                           WGL_CONTEXT_FLAGS_ARB, WGL_CONTEXT_FORWARD_COMPATIBLE_BIT_ARB | WGL_CONTEXT_DEBUG_BIT_ARB,
                           WGL_CONTEXT_PROFILE_MASK_ARB, WGL_CONTEXT_CORE_PROFILE_BIT_ARB, 0};
    HGLRC core = create(dc, NULL, attribs);
    CHECK("3.2 core debug context", core != NULL);
    if (!core) return 1;
    wglMakeCurrent(dc, core);
    wglDeleteContext(legacy);
    p_callback = (callback_t)wglGetProcAddress("glDebugMessageCallbackARB");
    if (!p_callback) p_callback = (callback_t)wglGetProcAddress("glDebugMessageCallback");
    p_insert = (insert_t)wglGetProcAddress("glDebugMessageInsertARB");
    if (!p_insert) p_insert = (insert_t)wglGetProcAddress("glDebugMessageInsert");
    CHECK("debug entry points", p_callback && p_insert);
    if (!p_callback || !p_insert) return 1;
    static const int tag = 1;
    p_callback(debug_proc, &tag);

    /* On the program's main thread, and on a thread of its own (wined3d's
     * command-stream thread is one): a Unix call's Unix side runs on the
     * calling thread's own stack, just below the program's frames. */
    wglMakeCurrent(NULL, NULL);
    for (int on_thread = 0; on_thread < 2; on_thread++) {
        struct run r = {dc, core, &tag, on_thread ? "thread" : "main thread"};
        if (on_thread) {
            HANDLE h = CreateThread(NULL, 0, run_modes, &r, 0, NULL);
            WaitForSingleObject(h, INFINITE);
            CloseHandle(h);
        } else {
            run_modes(&r);
        }
    }
    /* Callbacks nested until there is no room: each GL debug message inserts
     * the next from its callback. The chain must end with the Unix side
     * refusing one (STATUS_STACK_OVERFLOW: the insert returns without a
     * call) near the end of the thread's stack, which the program's frames
     * and the Unix side's share, not below it, and the program must still be
     * here. */
    {
        char what[160];
        struct run r = {dc, core, &tag, "nesting"};
        MEMORY_BASIC_INFORMATION mi = {0};
        HANDLE h = CreateThread(NULL, 0, run_nest, &r, 0, NULL);
        WaitForSingleObject(h, INFINITE);
        CloseHandle(h);
        if (nest_low) VirtualQuery(nest_low, &mi, sizeof(mi));
        snprintf(what, sizeof(what), "nested callbacks: %d deep from 48 KiB, the deepest %d bytes above the stack's end",
                 nest_depth, nest_low ? (int)(nest_low - (char *)mi.AllocationBase) : -1);
        CHECK(what, nest_depth >= 4 && nest_low && nest_low - (char *)mi.AllocationBase < (16 << 10));
    }
    wglMakeCurrent(dc, core);
    wglMakeCurrent(dc, core);
    p_callback(NULL, NULL);
    if (!failures) printf("usercb ok\n");
    else printf("usercb: %d failed\n", failures);
    fflush(stdout);
    wglMakeCurrent(NULL, NULL);
    wglDeleteContext(core);
    DestroyWindow(w);
    return failures != 0;
}
