/* A core-profile GL context for a 32-bit Wine program, as wined3d makes one
 * (test/wow64/glcore): wglCreateContextAttribsARB for 3.2 core, a GLSL
 * program built in wined3d's order (both shaders attached before they are
 * compiled) and in the usual one, an RGBA8 texture as an FBO's colour
 * attachment, a triangle drawn into it and read back. One "ok"/"FAIL" line a
 * step; "glcore ok" when every step passed. Natively and in the page. */
#include <windows.h>
#include <GL/gl.h>
#include <stdio.h>
#include <string.h>

#define WGL_CONTEXT_MAJOR_VERSION_ARB 0x2091
#define WGL_CONTEXT_MINOR_VERSION_ARB 0x2092
#define WGL_CONTEXT_FLAGS_ARB 0x2094
#define WGL_CONTEXT_PROFILE_MASK_ARB 0x9126
#define WGL_CONTEXT_CORE_PROFILE_BIT_ARB 0x0001
#define WGL_CONTEXT_FORWARD_COMPATIBLE_BIT_ARB 0x0002
#define GL_FRAGMENT_SHADER 0x8B30
#define GL_VERTEX_SHADER 0x8B31
#define GL_COMPILE_STATUS 0x8B81
#define GL_LINK_STATUS 0x8B82
#define GL_FRAMEBUFFER 0x8D40
#define GL_COLOR_ATTACHMENT0 0x8CE0
#define GL_FRAMEBUFFER_COMPLETE 0x8CD5
#define GL_ARRAY_BUFFER 0x8892
#define GL_STATIC_DRAW 0x88E4
#define GL_READ_FRAMEBUFFER 0x8CA8
#define GL_DRAW_FRAMEBUFFER 0x8CA9
#define GL_BGRA 0x80E1
#define GL_TEXTURE_3D 0x806F

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok  " : "FAIL", name); fflush(stdout); if (!ok_) failures++; } while (0)

typedef HGLRC (WINAPI *wglCreateContextAttribsARB_t)(HDC, HGLRC, const int *);
#define PROC(ret, name, args) typedef ret (WINAPI *name##_t) args; static name##_t p##name;
PROC(GLuint, glCreateShader, (GLenum))
PROC(void, glShaderSource, (GLuint, GLsizei, const char *const *, const GLint *))
PROC(void, glCompileShader, (GLuint))
PROC(void, glGetShaderiv, (GLuint, GLenum, GLint *))
PROC(void, glGetShaderInfoLog, (GLuint, GLsizei, GLsizei *, char *))
PROC(GLuint, glCreateProgram, (void))
PROC(void, glAttachShader, (GLuint, GLuint))
PROC(void, glGetAttachedShaders, (GLuint, GLsizei, GLsizei *, GLuint *))
PROC(void, glBindAttribLocation, (GLuint, GLuint, const char *))
PROC(void, glLinkProgram, (GLuint))
PROC(void, glGetProgramiv, (GLuint, GLenum, GLint *))
PROC(void, glGetProgramInfoLog, (GLuint, GLsizei, GLsizei *, char *))
PROC(void, glUseProgram, (GLuint))
PROC(void, glGenFramebuffers, (GLsizei, GLuint *))
PROC(void, glBindFramebuffer, (GLenum, GLuint))
PROC(void, glFramebufferTexture2D, (GLenum, GLenum, GLenum, GLuint, GLint))
PROC(GLenum, glCheckFramebufferStatus, (GLenum))
PROC(void, glGetInternalformativ, (GLenum, GLenum, GLenum, GLsizei, GLint *))
PROC(void, glFramebufferTexture1D, (GLenum, GLenum, GLenum, GLuint, GLint))
PROC(void, glFramebufferTexture3D, (GLenum, GLenum, GLenum, GLuint, GLint, GLint))
PROC(void, glTexImage3D, (GLenum, GLint, GLint, GLsizei, GLsizei, GLsizei, GLint, GLenum, GLenum, const void *))
PROC(void, glGenVertexArrays, (GLsizei, GLuint *))
PROC(void, glBindVertexArray, (GLuint))
PROC(void, glGenBuffers, (GLsizei, GLuint *))
PROC(void, glBindBuffer, (GLenum, GLuint))
PROC(void, glBufferData, (GLenum, ptrdiff_t, const void *, GLenum))
PROC(void, glVertexAttribPointer, (GLuint, GLint, GLenum, GLboolean, GLsizei, const void *))
PROC(void, glEnableVertexAttribArray, (GLuint))
PROC(void, glBlitFramebuffer, (GLint, GLint, GLint, GLint, GLint, GLint, GLint, GLint, GLbitfield, GLenum))
#define LOAD(name) p##name = (name##_t)wglGetProcAddress(#name)

static const char *vs_src = "#version 150 core\nin vec2 p;\nvoid main() { gl_Position = vec4(p, 0.0, 1.0); }\n";
static const char *fs_src = "#version 150 core\nout vec4 c;\nvoid main() { c = vec4(1.0, 0.0, 0.0, 1.0); }\n";

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM wp, LPARAM lp) { return DefWindowProcW(w, m, wp, lp); }

static void say_log(GLuint o, int program) {
    char log[512] = "";
    if (program) pglGetProgramInfoLog(o, sizeof(log), NULL, log);
    else pglGetShaderInfoLog(o, sizeof(log), NULL, log);
    if (log[0]) printf("  log: %s\n", log);
}

/* attach_first: wined3d's order (create, attach, attach, compile, compile, link). */
static GLuint build(int attach_first, const char *name) {
    char what[96];
    GLint st = 0;
    GLuint prog = pglCreateProgram();
    GLuint vs = pglCreateShader(GL_VERTEX_SHADER);
    if (attach_first) pglAttachShader(prog, vs);
    GLuint fs = pglCreateShader(GL_FRAGMENT_SHADER);
    if (attach_first) pglAttachShader(prog, fs);
    snprintf(what, sizeof(what), "%s: names (program %u, shaders %u %u)", name, prog, vs, fs);
    CHECK(what, prog && vs && fs);
    pglShaderSource(vs, 1, &vs_src, NULL);
    pglShaderSource(fs, 1, &fs_src, NULL);
    pglCompileShader(vs);
    pglCompileShader(fs);
    pglGetShaderiv(vs, GL_COMPILE_STATUS, &st);
    snprintf(what, sizeof(what), "%s: vertex shader compiles", name);
    CHECK(what, st);
    if (!st) say_log(vs, 0);
    pglGetShaderiv(fs, GL_COMPILE_STATUS, &st);
    snprintf(what, sizeof(what), "%s: fragment shader compiles", name);
    CHECK(what, st);
    if (!st) say_log(fs, 0);
    if (!attach_first) { pglAttachShader(prog, vs); pglAttachShader(prog, fs); }
    GLuint got[4] = {0};
    GLsizei n = 0;
    pglGetAttachedShaders(prog, 4, &n, got);
    snprintf(what, sizeof(what), "%s: attached shaders (%d: %u %u)", name, (int)n, got[0], got[1]);
    CHECK(what, n == 2 && ((got[0] == vs && got[1] == fs) || (got[0] == fs && got[1] == vs)));
    pglBindAttribLocation(prog, 0, "p");
    pglLinkProgram(prog);
    pglGetProgramiv(prog, GL_LINK_STATUS, &st);
    snprintf(what, sizeof(what), "%s: program links", name);
    CHECK(what, st);
    if (!st) say_log(prog, 1);
    return st ? prog : 0;
}

/* For the "thread" mode: what the presenting thread needs from main. */
static wglCreateContextAttribsARB_t g_create;
static const int *g_attribs;
static HGLRC g_core;
static HWND g_w;
static GLuint g_tex;
static volatile LONG g_thread_done;

/* Blit the 64x64 FBO into the window, flipped, and present, for 60 s. */
static void present_loop(HWND w, HDC dc, GLuint fbo, int pump, const char *who) {
    DWORD until = GetTickCount() + 60000;
    unsigned frames = 0;
    while ((int)(until - GetTickCount()) > 0) {
        MSG m;
        while (pump && PeekMessageW(&m, 0, 0, 0, PM_REMOVE)) { TranslateMessage(&m); DispatchMessageW(&m); }
        RECT r; GetClientRect(w, &r);
        pglBindFramebuffer(GL_DRAW_FRAMEBUFFER, 0);
        glClearColor(0, 0, 1, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        pglBindFramebuffer(GL_READ_FRAMEBUFFER, fbo);
        pglBlitFramebuffer(0, 0, 64, 64, 0, r.bottom, r.right, 0, GL_COLOR_BUFFER_BIT, GL_NEAREST);
        SwapBuffers(dc);
        if (++frames % 300 == 0) { printf("hold (%s): %u frames, client %ldx%ld, error %#x\n", who, frames, r.right, r.bottom, glGetError()); fflush(stdout); }
        Sleep(16);
    }
}

/* wined3d's shape: a second context sharing the first, current on another
 * thread, drawing into a window that thread does not own. */
static DWORD WINAPI present_thread(void *arg) {
    HDC dc = GetDC(g_w);
    HGLRC ctx = g_create(dc, g_core, g_attribs);
    CHECK("thread: shared context", ctx != NULL);
    CHECK("thread: make current", ctx && wglMakeCurrent(dc, ctx));
    GLuint fbo = 0;
    unsigned char px[4] = {0};
    pglGenFramebuffers(1, &fbo);
    pglBindFramebuffer(GL_FRAMEBUFFER, fbo);
    pglFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, g_tex, 0);
    glReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    char what[96];
    snprintf(what, sizeof(what), "thread: the shared texture reads back (%u %u %u %u)", px[0], px[1], px[2], px[3]);
    CHECK(what, px[0] > 200 && px[1] < 50 && px[2] < 50);
    if (arg) present_loop(g_w, dc, fbo, 0, "thread");
    wglMakeCurrent(NULL, NULL);
    wglDeleteContext(ctx);
    InterlockedExchange(&g_thread_done, 1);
    return 0;
}

/* wined3d's hidden helper windows: a 10x10 overlapped window with a GL
 * context, made current once. The caps probe destroys its window; the
 * backup DC window stays. */
static HWND fake_window(int keep) {
    HWND f = CreateWindowW(L"gex_glcore", L"glcore fake", WS_OVERLAPPEDWINDOW, 10, 10, 10, 10, 0, 0, GetModuleHandleW(0), 0);
    HDC dc = GetDC(f);
    PIXELFORMATDESCRIPTOR pfd = {sizeof(pfd), 1, PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER, PFD_TYPE_RGBA, 32};
    SetPixelFormat(dc, ChoosePixelFormat(dc, &pfd), &pfd);
    HGLRC c = wglCreateContext(dc);
    wglMakeCurrent(dc, c);
    glGetString(GL_VERSION);
    wglMakeCurrent(NULL, NULL);
    wglDeleteContext(c);
    ReleaseDC(f, dc);
    if (!keep) { DestroyWindow(f); f = NULL; }
    return f;
}

/* Clear the bound framebuffer blue, draw the red program over a W x H
 * viewport, and read the middle back: an attachment that is "complete" but
 * cannot be rendered into reads blue (or errors). */
static void draw_check(GLuint prog, int w, int h, const char *name) {
    unsigned char px[4] = {0};
    char what[128];
    glViewport(0, 0, w, h);
    glClearColor(0, 0, 1, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    pglUseProgram(prog);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    glReadPixels(w / 2, h / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    GLenum err = glGetError();
    snprintf(what, sizeof(what), "%s: drawn red (%u %u %u %u, error %#x)", name, px[0], px[1], px[2], px[3], err);
    CHECK(what, px[0] > 200 && px[1] < 50 && px[2] < 50 && err == GL_NO_ERROR);
}

int main(void) {
    WNDCLASSW wc = {0};
    wc.style = CS_OWNDC;
    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleW(0);
    wc.lpszClassName = L"gex_glcore";
    /* "brush": GDI paints the client area white under the GL surface, as a
     * GameMaker window does; the GL picture must still be on top. */
    if (strstr(GetCommandLineA(), "brush")) wc.hbrBackground = (HBRUSH)GetStockObject(WHITE_BRUSH);
    RegisterClassW(&wc);
    int fake = strstr(GetCommandLineA(), "fake") != NULL;
    if (fake) { fake_window(0); fake_window(1); }
    HWND w = CreateWindowW(L"gex_glcore", L"glcore", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 0, 0, 160, 160, 0, 0, wc.hInstance, 0);
    HDC dc = GetDC(w);
    PIXELFORMATDESCRIPTOR pfd = {sizeof(pfd), 1, PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER, PFD_TYPE_RGBA, 32};
    pfd.cDepthBits = 24;
    SetPixelFormat(dc, ChoosePixelFormat(dc, &pfd), &pfd);
    HGLRC legacy = wglCreateContext(dc);
    wglMakeCurrent(dc, legacy);
    wglCreateContextAttribsARB_t create = (wglCreateContextAttribsARB_t)wglGetProcAddress("wglCreateContextAttribsARB");
    CHECK("wglCreateContextAttribsARB", create != NULL);
    if (!create) return 1;
    const int attribs[] = {WGL_CONTEXT_MAJOR_VERSION_ARB, 3, WGL_CONTEXT_MINOR_VERSION_ARB, 2,
                           WGL_CONTEXT_FLAGS_ARB, WGL_CONTEXT_FORWARD_COMPATIBLE_BIT_ARB,
                           WGL_CONTEXT_PROFILE_MASK_ARB, WGL_CONTEXT_CORE_PROFILE_BIT_ARB, 0};
    HGLRC core = create(dc, NULL, attribs);
    CHECK("3.2 core context", core != NULL);
    if (!core) return 1;
    wglMakeCurrent(dc, core);
    wglDeleteContext(legacy);
    printf("  GL_VERSION %s\n  GL_RENDERER %s\n", glGetString(GL_VERSION), glGetString(GL_RENDERER));
    LOAD(glCreateShader); LOAD(glShaderSource); LOAD(glCompileShader); LOAD(glGetShaderiv); LOAD(glGetShaderInfoLog);
    LOAD(glCreateProgram); LOAD(glAttachShader); LOAD(glGetAttachedShaders); LOAD(glBindAttribLocation);
    LOAD(glLinkProgram); LOAD(glGetProgramiv); LOAD(glGetProgramInfoLog); LOAD(glUseProgram);
    LOAD(glGenFramebuffers); LOAD(glBindFramebuffer); LOAD(glFramebufferTexture2D); LOAD(glCheckFramebufferStatus);
    LOAD(glGetInternalformativ); LOAD(glFramebufferTexture1D); LOAD(glFramebufferTexture3D); LOAD(glTexImage3D);
    LOAD(glGenVertexArrays); LOAD(glBindVertexArray); LOAD(glGenBuffers); LOAD(glBindBuffer); LOAD(glBufferData);
    LOAD(glVertexAttribPointer); LOAD(glEnableVertexAttribArray); LOAD(glBlitFramebuffer);
    CHECK("entry points", pglCreateShader && pglGetAttachedShaders && pglCheckFramebufferStatus && pglGenVertexArrays);

    GLuint usual = build(0, "usual order");
    GLuint wined3d = build(1, "wined3d order");

    GLuint tex = 0, fbo = 0, vao = 0, vbo = 0;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 64, 64, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    pglGenFramebuffers(1, &fbo);
    pglBindFramebuffer(GL_FRAMEBUFFER, fbo);
    pglFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    GLenum status = pglCheckFramebufferStatus(GL_FRAMEBUFFER);
    char what[96];
    snprintf(what, sizeof(what), "RGBA8 texture FBO complete (%#x)", status);
    CHECK(what, status == GL_FRAMEBUFFER_COMPLETE);
    const float tri[] = {-1, -1, 3, -1, -1, 3};
    pglGenVertexArrays(1, &vao);
    pglBindVertexArray(vao);
    pglGenBuffers(1, &vbo);
    pglBindBuffer(GL_ARRAY_BUFFER, vbo);
    pglBufferData(GL_ARRAY_BUFFER, sizeof(tri), tri, GL_STATIC_DRAW);
    pglVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, 0);
    pglEnableVertexAttribArray(0);
    for (int i = 0; i < 2; i++) {
        GLuint prog = i ? wined3d : usual;
        unsigned char px[4] = {0};
        glViewport(0, 0, 64, 64);
        glClearColor(0, 0, 1, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        if (prog) {
            pglUseProgram(prog);
            glDrawArrays(GL_TRIANGLES, 0, 3);
        }
        glReadPixels(32, 32, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        snprintf(what, sizeof(what), "%s: drawn red into the FBO (%u %u %u %u)", i ? "wined3d order" : "usual order",
                 px[0], px[1], px[2], px[3]);
        CHECK(what, px[0] > 200 && px[1] < 50 && px[2] < 50);
    }
    GLenum err = glGetError();
    snprintf(what, sizeof(what), "no GL error (%#x)", err);
    CHECK(what, err == GL_NO_ERROR);

    /* wined3d's format probe (check_fbo_compat): a 16-texel 1D texture and a
     * 16x16x16 3D one as the only colour attachment. When these are
     * incomplete every format loses its render-target cap for 1D and 3D, and
     * D3D11's CreateTexture1D of a render target fails (Hades). */
    for (int i = 0; i < 2; i++) {
        const GLenum fmt = i ? GL_BGRA : GL_RGBA;
        GLuint t1 = 0, t3 = 0, f = 0;
        pglGenFramebuffers(1, &f);
        pglBindFramebuffer(GL_FRAMEBUFFER, f);
        glGenTextures(1, &t1);
        glBindTexture(GL_TEXTURE_1D, t1);
        glTexImage1D(GL_TEXTURE_1D, 0, GL_RGBA8, 16, 0, fmt, GL_UNSIGNED_BYTE, NULL);
        glTexParameteri(GL_TEXTURE_1D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        pglFramebufferTexture1D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_1D, t1, 0);
        status = pglCheckFramebufferStatus(GL_FRAMEBUFFER);
        snprintf(what, sizeof(what), "1D RGBA8 texture FBO complete (%s data, %#x, error %#x)", i ? "BGRA" : "RGBA", status, glGetError());
        CHECK(what, status == GL_FRAMEBUFFER_COMPLETE);
        draw_check(usual, 16, 1, i ? "1D (BGRA data)" : "1D (RGBA data)");
        pglFramebufferTexture1D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_1D, 0, 0);
        glGenTextures(1, &t3);
        glBindTexture(GL_TEXTURE_3D, t3);
        pglTexImage3D(GL_TEXTURE_3D, 0, GL_RGBA8, 16, 16, 16, 0, fmt, GL_UNSIGNED_BYTE, NULL);
        glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        pglFramebufferTexture3D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_3D, t3, 0, 0);
        status = pglCheckFramebufferStatus(GL_FRAMEBUFFER);
        snprintf(what, sizeof(what), "3D RGBA8 texture FBO complete (%s data, %#x, error %#x)", i ? "BGRA" : "RGBA", status, glGetError());
        CHECK(what, status == GL_FRAMEBUFFER_COMPLETE);
        draw_check(usual, 16, 16, i ? "3D slice 0 (BGRA data)" : "3D slice 0 (RGBA data)");
        pglBindFramebuffer(GL_FRAMEBUFFER, fbo);
        glDeleteTextures(1, &t1);
        glDeleteTextures(1, &t3);
    }
    /* ...and, where ARB_internalformat_query2 is there, wined3d asks this
     * instead of attaching (init_format_fbo_compat_info): it has to agree
     * with the completeness checks above. */
    if (pglGetInternalformativ) {
        static const GLenum targets[] = {GL_TEXTURE_1D, GL_TEXTURE_2D, GL_TEXTURE_3D};
        static const char *const names[] = {"1D", "2D", "3D"};
        for (int i = 0; i < 3; i++) {
            GLint v = -1;
            pglGetInternalformativ(targets[i], GL_RGBA8, 0x8289 /* GL_FRAMEBUFFER_RENDERABLE */, 1, &v);
            snprintf(what, sizeof(what), "%s RGBA8 framebuffer renderable by query (%#x, error %#x)", names[i], v, glGetError());
            CHECK(what, v == 0x82B7 /* GL_FULL_SUPPORT */);
        }
    }
    glViewport(0, 0, 64, 64);

    /* wined3d presents by blitting its offscreen target into the window's back
     * buffer, flipped in Y (its targets are upside down). The FBO: bottom half
     * red, top half green; each blit is read back from the default framebuffer. */
    pglBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glClearColor(1, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glEnable(GL_SCISSOR_TEST);
    glScissor(0, 32, 64, 32);
    glClearColor(0, 1, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glDisable(GL_SCISSOR_TEST);
    static const struct { const char *name; int sy0, sy1, dx1, dy0, dy1, sx1; GLenum filter; int want_green; } blits[] = {
        {"blit to the window", 0, 64, 64, 0, 64, 64, GL_NEAREST, 0},
        {"blit to the window, flipped in Y", 0, 64, 64, 64, 0, 64, GL_NEAREST, 1},
        {"blit to the window, scaled 2x linear", 0, 32, 96, 0, 96, 48, GL_LINEAR, 0},
        {"blit to the window, scaled and flipped", 0, 64, 96, 96, 0, 64, GL_LINEAR, 1},
    };
    for (unsigned i = 0; i < sizeof(blits) / sizeof(blits[0]); i++) {
        unsigned char px[4] = {0};
        pglBindFramebuffer(GL_DRAW_FRAMEBUFFER, 0);
        glDrawBuffer(GL_BACK);
        glClearColor(0, 0, 1, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        pglBindFramebuffer(GL_READ_FRAMEBUFFER, fbo);
        pglBlitFramebuffer(0, blits[i].sy0, blits[i].sx1, blits[i].sy1, 0, blits[i].dy0, blits[i].dx1, blits[i].dy1,
                           GL_COLOR_BUFFER_BIT, blits[i].filter);
        GLenum berr = glGetError();
        pglBindFramebuffer(GL_READ_FRAMEBUFFER, 0);
        glReadBuffer(GL_BACK);
        glReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        snprintf(what, sizeof(what), "%s (error %#x, pixel %u %u %u %u)", blits[i].name, berr, px[0], px[1], px[2], px[3]);
        CHECK(what, berr == GL_NO_ERROR && (blits[i].want_green ? px[1] > 200 && px[0] < 50 : px[0] > 200 && px[1] < 50) && px[2] < 50);
    }
    SwapBuffers(dc);
    err = glGetError();
    snprintf(what, sizeof(what), "no GL error after the blits (%#x)", err);
    CHECK(what, err == GL_NO_ERROR);

    /* The same from a second, shared context on another thread (wined3d's
     * command-stream thread). "hold" keeps presenting for a minute, from this
     * thread or, with "thread", from the other one. */
    int hold = strstr(GetCommandLineA(), "hold") != NULL;
    g_create = create; g_attribs = attribs; g_core = core; g_w = w; g_tex = tex;
    if (hold && !strstr(GetCommandLineA(), "thread")) {
        present_loop(w, dc, fbo, 1, "main");
    } else {
        glFinish();
        wglMakeCurrent(NULL, NULL);
        HANDLE t = CreateThread(NULL, 0, present_thread, hold ? (void *)1 : NULL, 0, NULL);
        while (!g_thread_done) {
            MSG m;
            while (PeekMessageW(&m, 0, 0, 0, PM_REMOVE)) { TranslateMessage(&m); DispatchMessageW(&m); }
            MsgWaitForMultipleObjects(1, &t, FALSE, 50, QS_ALLINPUT);
        }
        CloseHandle(t);
        wglMakeCurrent(dc, core);
    }
    err = glGetError();
    snprintf(what, sizeof(what), "no GL error at the end (%#x)", err);
    CHECK(what, err == GL_NO_ERROR);
    if (!failures) printf("glcore ok\n");
    else printf("glcore: %d failed\n", failures);
    fflush(stdout);
    wglMakeCurrent(NULL, NULL);
    wglDeleteContext(core);
    DestroyWindow(w);
    return failures != 0;
}
