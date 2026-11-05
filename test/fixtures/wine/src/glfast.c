/* The GL calls the unix-call dispatcher's provider writes into the command
 * ring itself (gex-cgen/providers/nt_unixcall.c): glEnable, glDisable,
 * glActiveTexture, glUseProgram, glBindTexture, glUniform1i, glUniform2f,
 * glBlendFunc, glBlendFuncSeparate, glViewport, glTexParameteri,
 * glUniform1f, glEnableClientState, glDisableClientState, glVertexPointer,
 * glColorPointer, glTexCoordPointer (into client memory) and glDrawArrays (which also leave the arrays'
 * flags where the layer reads them before a draw: client-memory arrays are
 * uploaded only when the layer sees them enabled).
 * Each is checked by a query (which the NT layer answers after a submit),
 * interleaved with calls the layer encodes, and many enough of them to fill
 * the ring several times over. Needs `gexc run --gl`; meaningful with
 * --cgen (the provider binds with the trampolines' unit).
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld -lgdi32 -lopengl32 */
#include <windows.h>
#include <GL/gl.h>
#include <stdio.h>

#define GL_TEXTURE0 0x84C0
#define GL_ACTIVE_TEXTURE 0x84E0
#define GL_CURRENT_PROGRAM 0x8B8D
#define GL_TEXTURE1 0x84C1

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

#define PROC(ret, name, args) typedef ret (WINAPI *name##_t) args; static name##_t p##name;
PROC(void, glActiveTexture, (GLenum))
PROC(GLuint, glCreateShader, (GLenum))
PROC(void, glShaderSource, (GLuint, GLsizei, const char *const *, const GLint *))
PROC(void, glCompileShader, (GLuint))
PROC(GLuint, glCreateProgram, (void))
PROC(void, glAttachShader, (GLuint, GLuint))
PROC(void, glLinkProgram, (GLuint))
PROC(void, glUseProgram, (GLuint))
PROC(GLint, glGetUniformLocation, (GLuint, const char *))
PROC(void, glUniform1i, (GLint, GLint))
PROC(void, glUniform2f, (GLint, float, float))
PROC(void, glUniform1f, (GLint, float))
PROC(void, glBlendFuncSeparate, (GLenum, GLenum, GLenum, GLenum))
PROC(void, glGetUniformfv, (GLuint, GLint, float *))
PROC(void, glGetUniformiv, (GLuint, GLint, GLint *))
PROC(void, glClientActiveTexture, (GLenum))
PROC(void, glGenBuffers, (GLsizei, GLuint *))
PROC(void, glBindBuffer, (GLenum, GLuint))
PROC(void, glBufferData, (GLenum, ptrdiff_t, const void *, GLenum))
#define LOAD(name) p##name = (name##_t)wglGetProcAddress(#name)

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM wp, LPARAM lp) { return DefWindowProcW(w, m, wp, lp); }

int main(void) {
    WNDCLASSW wc = {0};
    wc.style = CS_OWNDC;
    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleW(0);
    wc.lpszClassName = L"gex_glfast";
    RegisterClassW(&wc);
    HWND w = CreateWindowExW(0, L"gex_glfast", L"glfast", WS_POPUP, 0, 0, 64, 64, 0, 0, wc.hInstance, 0);
    HDC dc = GetDC(w);
    PIXELFORMATDESCRIPTOR pfd = {sizeof pfd, 1, PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER, PFD_TYPE_RGBA, 32};
    SetPixelFormat(dc, ChoosePixelFormat(dc, &pfd), &pfd);
    HGLRC gl = wglCreateContext(dc);
    CHECK("a context", gl && wglMakeCurrent(dc, gl));

    glEnable(GL_BLEND);
    CHECK("glEnable", glIsEnabled(GL_BLEND));
    glDisable(GL_BLEND);
    CHECK("glDisable", !glIsEnabled(GL_BLEND));

    GLuint tex[2];
    glGenTextures(2, tex);
    glBindTexture(GL_TEXTURE_2D, tex[1]);
    GLint b = 0;
    glGetIntegerv(GL_TEXTURE_BINDING_2D, &b);
    CHECK("glBindTexture", b == (GLint)tex[1]);

    LOAD(glActiveTexture); LOAD(glCreateShader); LOAD(glShaderSource); LOAD(glCompileShader);
    LOAD(glCreateProgram); LOAD(glAttachShader); LOAD(glLinkProgram); LOAD(glUseProgram);
    LOAD(glGetUniformLocation); LOAD(glUniform1i); LOAD(glUniform2f); LOAD(glUniform1f); LOAD(glBlendFuncSeparate); LOAD(glGetUniformfv); LOAD(glGetUniformiv);
    LOAD(glClientActiveTexture); LOAD(glGenBuffers); LOAD(glBindBuffer); LOAD(glBufferData);
    CHECK("GL 2.0 entry points", pglActiveTexture && pglUseProgram && pglUniform2f);
    if (!pglActiveTexture || !pglUseProgram || !pglUniform2f) return 1;

    pglActiveTexture(GL_TEXTURE0 + 3);
    GLint at = 0;
    glGetIntegerv(GL_ACTIVE_TEXTURE, &at);
    CHECK("glActiveTexture", at == GL_TEXTURE0 + 3);
    pglActiveTexture(GL_TEXTURE0);

    const char *vs = "#version 110\nvoid main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }";
    const char *fs = "#version 110\nuniform vec2 off; uniform int k; uniform float a; void main() { gl_FragColor = vec4(off, float(k), a); }";
    GLuint v = pglCreateShader(0x8B31), fr = pglCreateShader(0x8B30), prog = pglCreateProgram();
    pglShaderSource(v, 1, &vs, 0);
    pglShaderSource(fr, 1, &fs, 0);
    pglCompileShader(v);
    pglCompileShader(fr);
    pglAttachShader(prog, v);
    pglAttachShader(prog, fr);
    pglLinkProgram(prog);
    pglUseProgram(prog);
    /* (GL_CURRENT_PROGRAM answers the host's name for it, which the layer
     * does not map back: the uniforms below show the program is in use.) */
    GLint cur = 0;
    glGetIntegerv(GL_CURRENT_PROGRAM, &cur);
    CHECK("glUseProgram", cur != 0 && glGetError() == GL_NO_ERROR);
    GLint off = pglGetUniformLocation(prog, "off"), s = pglGetUniformLocation(prog, "k");
    pglUniform2f(off, 0.25f, -1.5f);
    pglUniform1i(s, 5);
    float fv[2] = {0, 0};
    GLint iv = 0;
    pglGetUniformfv(prog, off, fv);
    pglGetUniformiv(prog, s, &iv);
    CHECK("glUniform2f", fv[0] == 0.25f && fv[1] == -1.5f);
    CHECK("glUniform1i", iv == 5);
    if (pglUniform1f) {
        GLint a = pglGetUniformLocation(prog, "a");
        float av = 0;
        pglUniform1f(a, 0.75f);
        pglGetUniformfv(prog, a, &av);
        CHECK("glUniform1f", av == 0.75f);
    }
    glBlendFunc(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
    GLint bs = 0, bd = 0;
    glGetIntegerv(GL_BLEND_SRC, &bs);
    glGetIntegerv(GL_BLEND_DST, &bd);
    CHECK("glBlendFunc", bs == GL_SRC_ALPHA && bd == GL_ONE_MINUS_SRC_ALPHA);
    if (pglBlendFuncSeparate) {
        GLint v4[4] = {0, 0, 0, 0};
        pglBlendFuncSeparate(GL_ONE, GL_ZERO, GL_DST_ALPHA, GL_ONE);
        glGetIntegerv(0x80C9, &v4[0]); /* GL_BLEND_SRC_RGB */
        glGetIntegerv(0x80C8, &v4[1]); /* GL_BLEND_DST_RGB */
        glGetIntegerv(0x80CB, &v4[2]); /* GL_BLEND_SRC_ALPHA */
        glGetIntegerv(0x80CA, &v4[3]); /* GL_BLEND_DST_ALPHA */
        CHECK("glBlendFuncSeparate", v4[0] == GL_ONE && v4[1] == GL_ZERO && v4[2] == GL_DST_ALPHA && v4[3] == GL_ONE);
    }
    glViewport(3, 5, 40, 30);
    GLint vp[4] = {0, 0, 0, 0};
    glGetIntegerv(GL_VIEWPORT, vp);
    CHECK("glViewport", vp[0] == 3 && vp[1] == 5 && vp[2] == 40 && vp[3] == 30);
    glBindTexture(GL_TEXTURE_2D, tex[0]);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    GLint mf = 0;
    glGetTexParameteriv(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, &mf);
    CHECK("glTexParameteri", mf == GL_NEAREST);

    /* Two million fast calls (some 26 MB of commands: the ring fills and is
     * submitted three times), with layer-encoded calls among them, ending in
     * a known state. */
    for (int i = 0; i < 1000000; i++) {
        glEnable(GL_BLEND);
        if ((i & 4095) == 0) glClearColor(0, 0, 0, 1);
        glDisable(GL_BLEND);
    }
    for (int i = 0; i < 1000; i++) {
        glBindTexture(GL_TEXTURE_2D, tex[i & 1]);
        pglUniform2f(off, (float)i, 2.0f);
        pglUniform1i(s, i);
    }
    glEnable(GL_DEPTH_TEST);
    glGetIntegerv(GL_TEXTURE_BINDING_2D, &b);
    pglGetUniformfv(prog, off, fv);
    pglGetUniformiv(prog, s, &iv);
    CHECK("after the ring filled: the last of each", !glIsEnabled(GL_BLEND) && glIsEnabled(GL_DEPTH_TEST) &&
          b == (GLint)tex[1] && fv[0] == 999.0f && fv[1] == 2.0f && iv == 999);
    /* Client arrays: enabled and disabled on the fast path, drawn from
     * client memory by the layer (a red triangle over the window, then a
     * white one once the colour array is off). */
    pglUseProgram(0);
    glDisable(GL_DEPTH_TEST);
    glViewport(0, 0, 64, 64);
    static const float tri[6] = {-1, -1, 3, -1, -1, 3};
    static const unsigned char red[12] = {255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255};
    for (int i = 0; i < 300000; i++) {
        glEnableClientState(GL_VERTEX_ARRAY);
        glDisableClientState(GL_VERTEX_ARRAY);
    }
    glEnableClientState(GL_VERTEX_ARRAY);
    glEnableClientState(GL_COLOR_ARRAY);
    CHECK("glEnableClientState", glIsEnabled(GL_VERTEX_ARRAY) && glIsEnabled(GL_COLOR_ARRAY));
    glVertexPointer(2, GL_FLOAT, 0, tri);
    glColorPointer(4, GL_UNSIGNED_BYTE, 0, red);
    glClearColor(0, 0, 1, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    unsigned char px[4] = {0, 0, 0, 0};
    glReadPixels(32, 32, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    CHECK("a draw from client arrays enabled on the fast path", px[0] == 255 && px[1] == 0 && px[2] == 0);
    if (px[0] != 255 || px[1] || px[2]) printf("  pixel %d,%d,%d\n", px[0], px[1], px[2]);
    glDisableClientState(GL_COLOR_ARRAY);
    CHECK("glDisableClientState", glIsEnabled(GL_VERTEX_ARRAY) && !glIsEnabled(GL_COLOR_ARRAY));
    glColor4f(1, 1, 1, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    glReadPixels(32, 32, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    CHECK("the colour array off", px[0] == 255 && px[1] == 255 && px[2] == 255);
    if (px[1] != 255 || px[2] != 255) printf("  pixel %d,%d,%d\n", px[0], px[1], px[2]);
    /* Vertices from a buffer (the pointer set while it is bound: the layer's
     * command) and colours from client memory (set on the fast path once
     * it is unbound): the fast draw uploads only the client one. */
    if (pglGenBuffers && pglBindBuffer && pglBufferData) {
        static const unsigned char green[12] = {0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255};
        GLuint vbo = 0;
        pglGenBuffers(1, &vbo);
        pglBindBuffer(0x8892, vbo);
        pglBufferData(0x8892, sizeof tri, tri, 0x88E4);
        glVertexPointer(2, GL_FLOAT, 0, 0);
        pglBindBuffer(0x8892, 0);
        glEnableClientState(GL_COLOR_ARRAY);
        glColorPointer(4, GL_UNSIGNED_BYTE, 0, green);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, 3);
        glReadPixels(32, 32, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        CHECK("vertices from a buffer, colours from client memory", px[0] == 0 && px[1] == 255 && px[2] == 0);
        if (px[0] || px[1] != 255 || px[2]) printf("  pixel %d,%d,%d\n", px[0], px[1], px[2]);
        glDisableClientState(GL_COLOR_ARRAY);
        glVertexPointer(2, GL_FLOAT, 0, tri);
    }
    glDisableClientState(GL_VERTEX_ARRAY);
    /* The texture-coordinate array of the client active unit (which the
     * layer sets, and the fast path reads back). */
    if (pglClientActiveTexture) {
        pglClientActiveTexture(GL_TEXTURE1);
        glEnableClientState(GL_TEXTURE_COORD_ARRAY);
        int on1 = glIsEnabled(GL_TEXTURE_COORD_ARRAY);
        pglClientActiveTexture(GL_TEXTURE0);
        int on0 = glIsEnabled(GL_TEXTURE_COORD_ARRAY);
        pglClientActiveTexture(GL_TEXTURE1);
        glDisableClientState(GL_TEXTURE_COORD_ARRAY);
        int off1 = !glIsEnabled(GL_TEXTURE_COORD_ARRAY);
        pglClientActiveTexture(GL_TEXTURE0);
        CHECK("client texture units", on1 && !on0 && off1);
    }

    GLenum err = glGetError();
    CHECK("no GL error", err == GL_NO_ERROR);
    if (err) printf("  error %#x; blend %d depth %d texture %d off %g,%g s %d\n", err, glIsEnabled(GL_BLEND), glIsEnabled(GL_DEPTH_TEST), b, fv[0], fv[1], iv);

    wglMakeCurrent(0, 0);
    wglDeleteContext(gl);
    printf("%d failures\n", failures);
    return failures ? 1 : 0;
}
