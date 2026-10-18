/* glBufferSubData of the bytes a buffer already holds, as wined3d sends it
 * (tests/w3/glsubdata.sh): the page's Mesa skips such a write
 * (gex_wgpu_subdata_repeat), so every way the buffer can change in between
 * has to make the next identical write land again. A 3.2 core context, a
 * 4 MiB vertex buffer (above the 64 KiB under which the page keeps a CPU
 * shadow, which the skip never applies to) whose 8 KiB colour range feeds a
 * full-screen triangle,
 * drawn into an RGBA8 FBO and read back after each step:
 *   1. subdata A                          -> A
 *   2. subdata A again (the repeat)       -> A
 *   3. map-write B over the range, subdata A        -> A
 *   4. glCopyBufferSubData of B into the range (GPU), subdata A -> A
 *   5. subdata B, subdata A               -> A
 *   6. a different range, then A again    -> A
 *   7. A twice, then B                     -> B
 * Every step but 2 starts from a re-specified buffer whose last write is A,
 * so the skip is live going into each one. One "ok"/"FAIL" line a step; "glsubdata ok" when all passed. Natively and
 * in the page. */
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
#define GL_ARRAY_BUFFER 0x8892
#define GL_COPY_READ_BUFFER 0x8F36
#define GL_COPY_WRITE_BUFFER 0x8F37
#define GL_DYNAMIC_DRAW 0x88E8
#define GL_MAP_WRITE_BIT 0x0002

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok  " : "FAIL", name); fflush(stdout); if (!ok_) failures++; } while (0)

typedef HGLRC (WINAPI *wglCreateContextAttribsARB_t)(HDC, HGLRC, const int *);
#define PROC(ret, name, args) typedef ret (WINAPI *name##_t) args; static name##_t p##name;
PROC(GLuint, glCreateShader, (GLenum))
PROC(void, glShaderSource, (GLuint, GLsizei, const char *const *, const GLint *))
PROC(void, glCompileShader, (GLuint))
PROC(void, glGetShaderiv, (GLuint, GLenum, GLint *))
PROC(GLuint, glCreateProgram, (void))
PROC(void, glAttachShader, (GLuint, GLuint))
PROC(void, glBindAttribLocation, (GLuint, GLuint, const char *))
PROC(void, glLinkProgram, (GLuint))
PROC(void, glGetProgramiv, (GLuint, GLenum, GLint *))
PROC(void, glUseProgram, (GLuint))
PROC(void, glGenFramebuffers, (GLsizei, GLuint *))
PROC(void, glBindFramebuffer, (GLenum, GLuint))
PROC(void, glFramebufferTexture2D, (GLenum, GLenum, GLenum, GLuint, GLint))
PROC(void, glGenVertexArrays, (GLsizei, GLuint *))
PROC(void, glBindVertexArray, (GLuint))
PROC(void, glGenBuffers, (GLsizei, GLuint *))
PROC(void, glBindBuffer, (GLenum, GLuint))
PROC(void, glBufferData, (GLenum, ptrdiff_t, const void *, GLenum))
PROC(void, glBufferSubData, (GLenum, ptrdiff_t, ptrdiff_t, const void *))
PROC(void *, glMapBufferRange, (GLenum, ptrdiff_t, ptrdiff_t, GLbitfield))
PROC(GLboolean, glUnmapBuffer, (GLenum))
PROC(void, glCopyBufferSubData, (GLenum, GLenum, ptrdiff_t, ptrdiff_t, ptrdiff_t))
PROC(void, glVertexAttribPointer, (GLuint, GLint, GLenum, GLboolean, GLsizei, const void *))
PROC(void, glEnableVertexAttribArray, (GLuint))
#define LOAD(name) p##name = (name##_t)wglGetProcAddress(#name)

static const char *vs_src = "#version 150 core\nin vec2 p;\nin vec4 col;\nflat out vec4 c0;\n"
                            "void main() { c0 = col; gl_Position = vec4(p, 0.0, 1.0); }\n";
static const char *fs_src = "#version 150 core\nflat in vec4 c0;\nout vec4 c;\nvoid main() { c = c0; }\n";

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM wp, LPARAM lp) { return DefWindowProcW(w, m, wp, lp); }

#define BUF_SIZE (4 << 20)
#define RANGE_OFF 16384
#define RANGE_SIZE 8192 /* above the page's 4 KiB floor for the skip */
static float colA[RANGE_SIZE / 4], colB[RANGE_SIZE / 4];

static void fill(float *dst, float r, float g, float b) {
    for (int i = 0; i < RANGE_SIZE / 16; i++) { dst[4 * i] = r; dst[4 * i + 1] = g; dst[4 * i + 2] = b; dst[4 * i + 3] = 1; }
}

/* Draw with colours from the range and say whether the middle reads A (red). */
static void expect_a(const char *name) {
    unsigned char px[4] = {0};
    char what[160];
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    glReadPixels(32, 32, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    GLenum err = glGetError();
    snprintf(what, sizeof(what), "%s: reads A (%u %u %u %u, error %#x)", name, px[0], px[1], px[2], px[3], err);
    CHECK(what, px[0] > 200 && px[1] < 50 && px[2] < 50 && err == GL_NO_ERROR);
}

/* A new resource for buf (glBufferData re-specifies it), its colour attribute
 * pointed at it again, and A written into the range. */
static void fresh(void)
{
    pglBufferData(GL_ARRAY_BUFFER, BUF_SIZE, NULL, GL_DYNAMIC_DRAW);
    pglVertexAttribPointer(1, 4, GL_FLOAT, GL_FALSE, 0, (const void *)(uintptr_t)RANGE_OFF);
    pglEnableVertexAttribArray(1);
    pglBufferSubData(GL_ARRAY_BUFFER, RANGE_OFF, RANGE_SIZE, colA);
}

int main(void) {
    WNDCLASSW wc = {0};
    wc.style = CS_OWNDC;
    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleW(0);
    wc.lpszClassName = L"gex_glsubdata";
    RegisterClassW(&wc);
    HWND w = CreateWindowW(L"gex_glsubdata", L"glsubdata", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 0, 0, 160, 160, 0, 0, wc.hInstance, 0);
    HDC dc = GetDC(w);
    PIXELFORMATDESCRIPTOR pfd = {sizeof(pfd), 1, PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER, PFD_TYPE_RGBA, 32};
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
    LOAD(glCreateShader); LOAD(glShaderSource); LOAD(glCompileShader); LOAD(glGetShaderiv);
    LOAD(glCreateProgram); LOAD(glAttachShader); LOAD(glBindAttribLocation); LOAD(glLinkProgram);
    LOAD(glGetProgramiv); LOAD(glUseProgram); LOAD(glGenFramebuffers); LOAD(glBindFramebuffer);
    LOAD(glFramebufferTexture2D); LOAD(glGenVertexArrays); LOAD(glBindVertexArray); LOAD(glGenBuffers);
    LOAD(glBindBuffer); LOAD(glBufferData); LOAD(glBufferSubData); LOAD(glMapBufferRange); LOAD(glUnmapBuffer);
    LOAD(glCopyBufferSubData); LOAD(glVertexAttribPointer); LOAD(glEnableVertexAttribArray);
    CHECK("entry points", pglBufferSubData && pglMapBufferRange && pglCopyBufferSubData && pglGenVertexArrays);

    GLint st = 0;
    GLuint prog = pglCreateProgram(), vs = pglCreateShader(GL_VERTEX_SHADER), fs = pglCreateShader(GL_FRAGMENT_SHADER);
    pglShaderSource(vs, 1, &vs_src, NULL);
    pglShaderSource(fs, 1, &fs_src, NULL);
    pglCompileShader(vs);
    pglCompileShader(fs);
    pglAttachShader(prog, vs);
    pglAttachShader(prog, fs);
    pglBindAttribLocation(prog, 0, "p");
    pglBindAttribLocation(prog, 1, "col");
    pglLinkProgram(prog);
    pglGetProgramiv(prog, GL_LINK_STATUS, &st);
    CHECK("program links", st);
    pglUseProgram(prog);

    GLuint tex = 0, fbo = 0, vao = 0, pos = 0, buf = 0, src = 0;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 64, 64, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    pglGenFramebuffers(1, &fbo);
    pglBindFramebuffer(GL_FRAMEBUFFER, fbo);
    pglFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    glViewport(0, 0, 64, 64);

    const float tri[] = {-1, -1, 3, -1, -1, 3};
    pglGenVertexArrays(1, &vao);
    pglBindVertexArray(vao);
    pglGenBuffers(1, &pos);
    pglBindBuffer(GL_ARRAY_BUFFER, pos);
    pglBufferData(GL_ARRAY_BUFFER, sizeof(tri), tri, GL_DYNAMIC_DRAW);
    pglVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, 0);
    pglEnableVertexAttribArray(0);

    fill(colA, 1, 0, 0);
    fill(colB, 0, 1, 0);
    /* B, for the GPU copy (a buffer of its own). */
    pglGenBuffers(1, &src);
    pglBindBuffer(GL_COPY_READ_BUFFER, src);
    pglBufferData(GL_COPY_READ_BUFFER, RANGE_SIZE, colB, GL_DYNAMIC_DRAW);

    pglGenBuffers(1, &buf);
    pglBindBuffer(GL_ARRAY_BUFFER, buf);
    pglBindBuffer(GL_COPY_WRITE_BUFFER, buf);

    /* Each step starts from a buffer of its own (fresh: re-specified, so a
     * new resource whose last write is A): a map or a GPU write leaves a
     * buffer the page never skips for again, which would hide every later
     * step from the skip. */
    fresh();
    expect_a("1. subdata A");
    pglBufferSubData(GL_ARRAY_BUFFER, RANGE_OFF, RANGE_SIZE, colA);
    expect_a("2. the same subdata again");

    fresh();
    void *m = pglMapBufferRange(GL_ARRAY_BUFFER, RANGE_OFF, RANGE_SIZE, GL_MAP_WRITE_BIT);
    CHECK("3. map for write", m != NULL);
    if (m) memcpy(m, colB, RANGE_SIZE);
    pglUnmapBuffer(GL_ARRAY_BUFFER);
    pglBufferSubData(GL_ARRAY_BUFFER, RANGE_OFF, RANGE_SIZE, colA);
    expect_a("3. map-write B, then subdata A");

    fresh();
    pglCopyBufferSubData(GL_COPY_READ_BUFFER, GL_COPY_WRITE_BUFFER, 0, RANGE_OFF, RANGE_SIZE);
    pglBufferSubData(GL_ARRAY_BUFFER, RANGE_OFF, RANGE_SIZE, colA);
    expect_a("4. GPU copy of B, then subdata A");

    fresh();
    pglBufferSubData(GL_ARRAY_BUFFER, RANGE_OFF, RANGE_SIZE, colB);
    pglBufferSubData(GL_ARRAY_BUFFER, RANGE_OFF, RANGE_SIZE, colA);
    expect_a("5. subdata B, then subdata A");

    fresh();
    pglBufferSubData(GL_ARRAY_BUFFER, 0, RANGE_SIZE, colB);
    pglBufferSubData(GL_ARRAY_BUFFER, RANGE_OFF, RANGE_SIZE, colA);
    pglBufferSubData(GL_ARRAY_BUFFER, RANGE_OFF, RANGE_SIZE, colA);
    expect_a("6. another range, then A twice");

    fresh();
    pglBufferSubData(GL_ARRAY_BUFFER, RANGE_OFF, RANGE_SIZE, colA); /* skipped */
    /* The skip must not hide a real change between draws either. */
    pglBufferSubData(GL_ARRAY_BUFFER, RANGE_OFF, RANGE_SIZE, colB);
    {
        unsigned char px[4] = {0};
        char what[160];
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, 3);
        glReadPixels(32, 32, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        snprintf(what, sizeof(what), "7. subdata B after A: reads B (%u %u %u %u)", px[0], px[1], px[2], px[3]);
        CHECK(what, px[0] < 50 && px[1] > 200 && px[2] < 50);
    }
    if (failures) { printf("glsubdata FAIL (%d)\n", failures); return 1; }
    printf("glsubdata ok\n");
    fflush(stdout);
    /* "hold": stay up, so a page run's harness can read the console. */
    if (strstr(GetCommandLineA(), "hold")) Sleep(60000);
    return 0;
}
