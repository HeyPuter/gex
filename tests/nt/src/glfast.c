/* The GL calls the unix-call dispatcher's provider writes into the command
 * ring itself (gex-cgen/providers/nt_unixcall.c): glEnable, glDisable,
 * glActiveTexture, glUseProgram, glBindTexture, glUniform1i, glUniform2f.
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
PROC(void, glGetUniformfv, (GLuint, GLint, float *))
PROC(void, glGetUniformiv, (GLuint, GLint, GLint *))
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
    LOAD(glGetUniformLocation); LOAD(glUniform1i); LOAD(glUniform2f); LOAD(glGetUniformfv); LOAD(glGetUniformiv);
    CHECK("GL 2.0 entry points", pglActiveTexture && pglUseProgram && pglUniform2f);
    if (!pglActiveTexture || !pglUseProgram || !pglUniform2f) return 1;

    pglActiveTexture(GL_TEXTURE0 + 3);
    GLint at = 0;
    glGetIntegerv(GL_ACTIVE_TEXTURE, &at);
    CHECK("glActiveTexture", at == GL_TEXTURE0 + 3);
    pglActiveTexture(GL_TEXTURE0);

    const char *vs = "#version 110\nvoid main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }";
    const char *fs = "#version 110\nuniform vec2 off; uniform int k; void main() { gl_FragColor = vec4(off, float(k), 1.0); }";
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
    GLenum err = glGetError();
    CHECK("no GL error", err == GL_NO_ERROR);
    if (err) printf("  error %#x; blend %d depth %d texture %d off %g,%g s %d\n", err, glIsEnabled(GL_BLEND), glIsEnabled(GL_DEPTH_TEST), b, fv[0], fv[1], iv);

    wglMakeCurrent(0, 0);
    wglDeleteContext(gl);
    printf("%d failures\n", failures);
    return failures ? 1 : 0;
}
