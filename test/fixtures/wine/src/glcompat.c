/* opengl32 through the generated compatibility-profile transport
 * (scripts/gl/gen_gl_compat.py): parameter vectors in and out, a pname-sized
 * query, queries and fences, a mapped buffer, client-memory vertex
 * attributes drawn with client-memory indices, read back. Needs `gexc run
 * --gl`.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld -lgdi32 -lopengl32 */
#include <windows.h>
#include <GL/gl.h>
#include <stdio.h>
#include <string.h>

#define GL_ARRAY_BUFFER 0x8892

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

typedef ptrdiff_t GLsizeiptr_;
typedef void *GLsync_;
#define PROC(ret, name, args) typedef ret (WINAPI *name##_t) args; static name##_t p##name;
PROC(void, glGenBuffers, (GLsizei, GLuint *))
PROC(void, glBindBuffer, (GLenum, GLuint))
PROC(void, glBufferData, (GLenum, GLsizeiptr_, const void *, GLenum))
PROC(void *, glMapBuffer, (GLenum, GLenum))
PROC(GLboolean, glUnmapBuffer, (GLenum))
PROC(void, glGetBufferSubData, (GLenum, ptrdiff_t, GLsizeiptr_, void *))
PROC(void, glGenQueries, (GLsizei, GLuint *))
PROC(void, glBeginQuery, (GLenum, GLuint))
PROC(void, glEndQuery, (GLenum))
PROC(GLboolean, glIsQuery, (GLuint))
PROC(void, glGetQueryObjectuiv, (GLuint, GLenum, GLuint *))
PROC(GLsync_, glFenceSync, (GLenum, GLbitfield))
PROC(GLenum, glClientWaitSync, (GLsync_, GLbitfield, unsigned long long))
PROC(void, glVertexAttribPointer, (GLuint, GLint, GLenum, GLboolean, GLsizei, const void *))
PROC(void, glEnableVertexAttribArray, (GLuint))
PROC(GLuint, glCreateShader, (GLenum))
PROC(void, glShaderSource, (GLuint, GLsizei, const char *const *, const GLint *))
PROC(void, glCompileShader, (GLuint))
PROC(GLuint, glCreateProgram, (void))
PROC(void, glAttachShader, (GLuint, GLuint))
PROC(void, glBindAttribLocation, (GLuint, GLuint, const char *))
PROC(void, glLinkProgram, (GLuint))
PROC(void, glUseProgram, (GLuint))
PROC(void, glGetProgramiv, (GLuint, GLenum, GLint *))
PROC(void, glUniform4f, (GLint, float, float, float, float))
PROC(GLint, glGetUniformLocation, (GLuint, const char *))
PROC(void, glGetUniformfv, (GLuint, GLint, float *))
PROC(void, glDeleteShader, (GLuint))
PROC(void, glTexStorage2D, (GLenum, GLsizei, GLenum, GLsizei, GLsizei))
PROC(void, glTextureView, (GLuint, GLenum, GLuint, GLenum, GLuint, GLuint, GLuint, GLuint))
#define LOAD(name) p##name = (name##_t)wglGetProcAddress(#name)

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM wp, LPARAM lp) { return DefWindowProcW(w, m, wp, lp); }

int main(void) {
    WNDCLASSW wc = {0};
    wc.style = CS_OWNDC;
    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleW(0);
    wc.lpszClassName = L"gex_glcompat";
    RegisterClassW(&wc);
    HWND w = CreateWindowExW(0, L"gex_glcompat", L"glcompat", WS_POPUP, 0, 0, 64, 64, 0, 0, wc.hInstance, 0);
    HDC dc = GetDC(w);
    PIXELFORMATDESCRIPTOR pfd = {sizeof pfd, 1, PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER, PFD_TYPE_RGBA, 32};
    /* BOOL results come back through the unix call's return slot (the
     * transport's Params::ret): a wrong slot reads as FALSE here. */
    CHECK("SetPixelFormat returns TRUE", SetPixelFormat(dc, ChoosePixelFormat(dc, &pfd), &pfd) == TRUE);
    HGLRC gl = wglCreateContext(dc);
    CHECK("a context", gl && wglMakeCurrent(dc, gl));
    ShowWindow(w, SW_SHOW);

    const char *version = (const char *)glGetString(GL_VERSION);
    CHECK("a version", version && version[0] >= '1' && version[0] <= '4' && version[1] == '.');

    /* A pname-sized vector, in and out. */
    float fog[4] = {0.25f, 0.5f, 0.75f, 1.0f}, got[4] = {0};
    glFogfv(GL_FOG_COLOR, fog);
    glGetFloatv(GL_FOG_COLOR, got);
    CHECK("glFogfv / glGetFloatv(GL_FOG_COLOR): four values", !memcmp(fog, got, sizeof fog));
    float spot[3] = {0, 0, -1}, sgot[4] = {9, 9, 9, 9};
    glLightfv(GL_LIGHT0, GL_SPOT_DIRECTION, spot);
    glGetLightfv(GL_LIGHT0, GL_SPOT_DIRECTION, sgot);
    CHECK("glGetLightfv(GL_SPOT_DIRECTION): three values, the fourth untouched", sgot[2] == -1 && sgot[3] == 9);
    float m[16];
    glMatrixMode(GL_MODELVIEW);
    glLoadIdentity();
    glTranslatef(1, 2, 3);
    glGetFloatv(GL_MODELVIEW_MATRIX, m);
    CHECK("glGetFloatv(GL_MODELVIEW_MATRIX): sixteen", m[12] == 1 && m[13] == 2 && m[14] == 3 && m[15] == 1);
    glLoadIdentity();
    GLdouble plane[4] = {1, 0, 0, -0.5}, pgot[4] = {0};
    glClipPlane(GL_CLIP_PLANE0, plane);
    glGetClipPlane(GL_CLIP_PLANE0, pgot);
    CHECK("glClipPlane / glGetClipPlane: doubles", pgot[0] == 1 && pgot[3] == -0.5);

    LOAD(glGenBuffers); LOAD(glBindBuffer); LOAD(glBufferData); LOAD(glMapBuffer); LOAD(glUnmapBuffer);
    LOAD(glGetBufferSubData); LOAD(glGenQueries); LOAD(glBeginQuery); LOAD(glEndQuery); LOAD(glIsQuery);
    LOAD(glGetQueryObjectuiv); LOAD(glFenceSync); LOAD(glClientWaitSync); LOAD(glVertexAttribPointer);
    LOAD(glEnableVertexAttribArray); LOAD(glCreateShader); LOAD(glShaderSource); LOAD(glCompileShader);
    LOAD(glCreateProgram); LOAD(glAttachShader); LOAD(glBindAttribLocation); LOAD(glLinkProgram);
    LOAD(glUseProgram); LOAD(glGetProgramiv); LOAD(glUniform4f); LOAD(glGetUniformLocation); LOAD(glGetUniformfv);
    LOAD(glDeleteShader); LOAD(glTexStorage2D); LOAD(glTextureView);
    CHECK("GL 2.0 entry points", pglMapBuffer && pglVertexAttribPointer && pglGenQueries && pglCreateProgram);
    if (!pglMapBuffer || !pglVertexAttribPointer || !pglGenQueries || !pglCreateProgram) goto out;

    /* A mapped buffer: written through the map, read back. */
    GLuint buf;
    unsigned char init[64], back[64];
    for (int i = 0; i < 64; i++) init[i] = (unsigned char)i;
    pglGenBuffers(1, &buf);
    pglBindBuffer(GL_ARRAY_BUFFER, buf);
    pglBufferData(GL_ARRAY_BUFFER, 64, init, 0x88E8 /* GL_DYNAMIC_DRAW */);
    unsigned char *map = pglMapBuffer(GL_ARRAY_BUFFER, 0x88BA /* GL_READ_WRITE */);
    CHECK("glMapBuffer: the contents", map && map[10] == 10 && map[63] == 63);
    if (map) map[10] = 0xaa;
    CHECK("glUnmapBuffer", pglUnmapBuffer(GL_ARRAY_BUFFER));
    memset(back, 0, sizeof back);
    pglGetBufferSubData(GL_ARRAY_BUFFER, 8, 8, back);
    CHECK("the write through the map reached the buffer", back[2] == 0xaa && back[3] == 11);
    pglBindBuffer(GL_ARRAY_BUFFER, 0);

    /* Queries and fences. */
    GLuint q;
    pglGenQueries(1, &q);
    pglBeginQuery(0x8914 /* GL_SAMPLES_PASSED */, q);
    pglEndQuery(0x8914);
    CHECK("glIsQuery", pglIsQuery(q));
    /* GL_QUERY_RESULT waits for the result; only after it is
     * GL_QUERY_RESULT_AVAILABLE guaranteed to be TRUE (asked right after
     * glEndQuery, the host's driver may still answer FALSE: the old check's
     * flake). Nothing was drawn, so no samples passed. */
    GLuint result = 0xdead, avail = 0;
    pglGetQueryObjectuiv(q, 0x8866 /* GL_QUERY_RESULT */, &result);
    pglGetQueryObjectuiv(q, 0x8867 /* GL_QUERY_RESULT_AVAILABLE */, &avail);
    CHECK("a query's result", result == 0 && avail == 1);
    GLsync_ f = pglFenceSync(0x9117, 0);
    GLenum ws = f ? pglClientWaitSync(f, 1, 1000000000ull) : 0;
    CHECK("a fence signals", ws == 0x911A || ws == 0x911C);

    /* A shader drawing client-memory attributes with client-memory indices. */
    const char *vs = "#version 110\nattribute vec2 pos; void main() { gl_Position = vec4(pos, 0.0, 1.0); }";
    const char *fs = "#version 110\nuniform vec4 color; void main() { gl_FragColor = color; }";
    GLuint v = pglCreateShader(0x8B31), fr = pglCreateShader(0x8B30), prog = pglCreateProgram();
    pglShaderSource(v, 1, &vs, 0);
    pglShaderSource(fr, 1, &fs, 0);
    pglCompileShader(v);
    pglCompileShader(fr);
    pglAttachShader(prog, v);
    pglAttachShader(prog, fr);
    pglBindAttribLocation(prog, 3, "pos");
    pglLinkProgram(prog);
    GLint linked = 0;
    pglGetProgramiv(prog, 0x8B82 /* GL_LINK_STATUS */, &linked);
    CHECK("a program links", linked);
    pglUseProgram(prog);
    GLint loc = pglGetUniformLocation(prog, "color");
    pglUniform4f(loc, 0, 1, 0, 1);
    float ugot[4] = {0};
    pglGetUniformfv(prog, loc, ugot);
    CHECK("glGetUniformfv: the uniform's four", ugot[1] == 1 && ugot[3] == 1);
    static const float quad[] = {-1, -1, 1, -1, 1, 1, -1, 1};
    static const unsigned short idx[] = {0, 1, 2, 0, 2, 3};
    glViewport(0, 0, 64, 64);
    glClearColor(1, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    pglVertexAttribPointer(3, 2, GL_FLOAT, GL_FALSE, 0, quad);
    pglEnableVertexAttribArray(3);
    glDrawElements(GL_TRIANGLES, 6, GL_UNSIGNED_SHORT, idx);
    unsigned char px[4] = {0};
    glReadPixels(32, 32, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    CHECK("a client-array, client-index draw", px[0] < 16 && px[1] > 240);
    pglUseProgram(0);

    /* A shader deleted while attached, compiled only afterwards (wined3d's
     * order): GL keeps it until it is detached, so the program still links. */
    GLuint v2 = pglCreateShader(0x8B31), f2 = pglCreateShader(0x8B30), prog2 = pglCreateProgram();
    pglShaderSource(v2, 1, &vs, 0);
    pglShaderSource(f2, 1, &fs, 0);
    pglAttachShader(prog2, v2);
    pglAttachShader(prog2, f2);
    pglDeleteShader(v2);
    pglDeleteShader(f2);
    pglCompileShader(v2);
    pglCompileShader(f2);
    pglLinkProgram(prog2);
    GLint linked2 = 0;
    pglGetProgramiv(prog2, 0x8B82 /* GL_LINK_STATUS */, &linked2);
    CHECK("a program links from shaders deleted while attached", linked2);

    /* A texture view: its name has to be one GL generated and never bound
     * (wined3d's D3D11 views). The guest names textures itself. */
    if (pglTexStorage2D && pglTextureView) {
        while (glGetError() != GL_NO_ERROR) {
        }
        GLuint t[2];
        glGenTextures(2, t);
        glBindTexture(GL_TEXTURE_2D, t[0]);
        pglTexStorage2D(GL_TEXTURE_2D, 1, 0x8058 /* GL_RGBA8 */, 4, 4);
        pglTextureView(t[1], GL_TEXTURE_2D, t[0], 0x8058, 0, 1, 0, 1);
        CHECK("glTextureView on a generated name", glGetError() == GL_NO_ERROR);
        glBindTexture(GL_TEXTURE_2D, 0);
    }

out:
    CHECK("no GL error", glGetError() == GL_NO_ERROR);
    CHECK("SwapBuffers returns TRUE", SwapBuffers(dc) == TRUE);
    wglMakeCurrent(0, 0);
    CHECK("wglDeleteContext returns TRUE", wglDeleteContext(gl) == TRUE);
    printf("%d failures\n", failures);
    return failures;
}
