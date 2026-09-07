/* The GL_INVALID_VALUE errors the program-interface queries owe.
 *
 * These are exactly the checks in KHR-GL44.program_interface_query's
 * invalid-value case, asked one at a time. That case fails in the browser and
 * passes natively and reports only "Fail", so the useful thing is to find out
 * *which* of its questions is answered wrongly rather than to guess.
 *
 * Two shapes are in here and they fail for different reasons:
 *
 *   - a bogus *program name*, which the transport used to map to zero and so
 *     answered whichever error zero earned rather than "not an object";
 *   - a negative *bufSize*, which for glGetProgramResourceName never crossed
 *     the wire at all: the guest clamped it locally and the host substituted
 *     its own buffer size, so GL never saw the caller's value.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

static int cases, passed;

static GLenum drain(void) {
    GLenum l = GL_NO_ERROR, e;
    while ((e = glGetError()) != GL_NO_ERROR) l = e;
    return l;
}

static void want_value(const char *name, GLenum got) {
    ++cases;
    const int ok = got == GL_INVALID_VALUE;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-42s got=0x%04x want=0x0501\n", name,
            (unsigned)got);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 430 core\n"
    "layout(location = 0) in vec4 position;\n"
    "void main() { gl_Position = position; }\n";
static const char *fs_src =
    "#version 430 core\n"
    "out vec4 color;\n"
    "void main() { color = vec4(1.0); }\n";

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) return 3;
    const EGLint sa[] = { EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 3;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    GLint res = 0, len = 0;
    char name[256];
    const GLenum props[1] = { GL_TYPE };
    drain();

    /* A name no glCreateProgram returned: "not an object at all", which GL
       distinguishes from an object of the wrong type. */
    glGetProgramInterfaceiv(1337u, GL_PROGRAM_INPUT, GL_ACTIVE_RESOURCES, &res);
    want_value("interfaceiv_bogus_program", drain());
    glGetProgramResourceIndex(31337u, GL_PROGRAM_INPUT, "pie");
    want_value("resourceindex_bogus_program", drain());
    glGetProgramResourceName(1337u, GL_PROGRAM_INPUT, 0, 1024, &len, name);
    want_value("resourcename_bogus_program", drain());
    glGetProgramResourceiv(1337u, GL_PROGRAM_INPUT, 0, 1, props, 1024, &len, &res);
    want_value("resourceiv_bogus_program", drain());
    glGetProgramResourceLocation(1337u, GL_PROGRAM_INPUT, "pie");
    want_value("resourcelocation_bogus_program", drain());
    glGetProgramResourceLocationIndex(1337u, GL_PROGRAM_OUTPUT, "pie");
    want_value("resourcelocationindex_bogus_program", drain());

    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL); glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL); glCompileShader(fs);
    GLuint program = glCreateProgram();
    glAttachShader(program, vs); glAttachShader(program, fs);
    glBindAttribLocation(program, 0, "position");
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) { printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }
    drain();

    /* An index past the interface's active resources. */
    glGetProgramResourceName(program, GL_PROGRAM_INPUT, 3000, 1024, &len, name);
    want_value("resourcename_index_out_of_range", drain());

    /* propCount of zero, which the specification calls out by itself. */
    glGetProgramResourceiv(program, GL_PROGRAM_INPUT, 0, 0, props, 1024,
                           &len, &res);
    want_value("resourceiv_propcount_zero", drain());

    /* And the negative buffer sizes. The one for glGetProgramResourceName is
       the interesting case: its bufSize never crossed the transport, so GL
       could not raise anything for it. */
    glGetProgramResourceName(program, GL_PROGRAM_INPUT, 0, -100, NULL, name);
    want_value("resourcename_negative_bufsize", drain());
    glGetProgramResourceiv(program, GL_PROGRAM_INPUT, 0, 1, props, -100,
                           &len, &res);
    want_value("resourceiv_negative_bufsize", drain());

    /* A valid query must still answer, so none of the above is a blanket
       refusal. */
    res = -1;
    glGetProgramInterfaceiv(program, GL_PROGRAM_INPUT, GL_ACTIVE_RESOURCES,
                            &res);
    const GLenum clean = drain();
    const int ok = clean == GL_NO_ERROR && res >= 1;
    ++cases; if (ok) ++passed;
    fprintf(stderr, "INFO %-42s error=0x%04x active=%d\n",
            "valid_query_still_answers", (unsigned)clean, res);
    printf("CASE valid_query_still_answers %s\n", ok ? "PASS" : "FAIL");

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
