#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
/* Legacy GLSL, which is what a compatibility profile is for.
 *
 * glmark2 -- and most GL2-era code -- writes shaders with no #version at all,
 * using attribute and varying. That is GLSL 1.10, legal only in a
 * compatibility profile. Native Mesa hands out 4.6 Compatibility when an
 * application asks for no particular version, so this compiles there; a core
 * profile rejects it.
 *
 * A GLSL 1.10 shader: no #version, attribute/varying. Legal only in a
 * compatibility profile, and what glmark2's whole shader set looks like. */
static const char *vs =
    "attribute vec3 position;\n"
    "varying vec2 vUv;\n"
    "void main() { vUv = position.xy; gl_Position = vec4(position, 1.0); }\n";
static const char *fs =
    "varying vec2 vUv;\n"
    "void main() { gl_FragColor = vec4(vUv, 0.0, 1.0); }\n";
int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (d == EGL_NO_DISPLAY || !eglInitialize(d, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint ca[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE,
                         EGL_OPENGL_BIT, EGL_RED_SIZE, 8, EGL_NONE};
    EGLConfig c = NULL; EGLint n = 0;
    if (!eglChooseConfig(d, ca, &c, 1, &n) || n < 1) return 2;
    const EGLint xa[] = {EGL_NONE};
    const EGLint sa[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, xa);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!eglMakeCurrent(d, s, s, x)) return 2;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));
    GLuint v = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(v, 1, &vs, NULL); glCompileShader(v);
    GLint ok = 0; glGetShaderiv(v, GL_COMPILE_STATUS, &ok);
    char log[400] = {0};
    if (!ok) glGetShaderInfoLog(v, sizeof(log) - 1, NULL, log);
    fprintf(stderr, "INFO legacy_vertex=%d %s\n", ok, log);
    GLuint f = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(f, 1, &fs, NULL); glCompileShader(f);
    GLint ok2 = 0; glGetShaderiv(f, GL_COMPILE_STATUS, &ok2);
    log[0] = 0;
    if (!ok2) glGetShaderInfoLog(f, sizeof(log) - 1, NULL, log);
    fprintf(stderr, "INFO legacy_fragment=%d %s\n", ok2, log);
    printf("SUMMARY compat=%d\n", ok && ok2);
    return 0;
}
