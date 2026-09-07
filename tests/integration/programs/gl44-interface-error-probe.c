/* The errors the program-interface queries owe.
 *
 * KHR-GL44.program_interface_query's invalid-enum, invalid-operation and
 * invalid-value cases fail only in the browser, and they log a call trace
 * rather than a reason -- they check glGetError and say nothing about which
 * call disagreed. This asks each question separately so the answer names
 * itself.
 *
 * The shape to expect: the guest answers some of this locally and forwards
 * the rest, and a query that returns early without raising anything looks
 * exactly like a query that succeeded. That was the whole of the
 * buffer_storage work earlier in this pass.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;
static void check(const char *name, GLenum got, GLenum want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-34s got=0x%04x want=0x%04x\n", name,
            (unsigned)got, (unsigned)want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}
/* For the questions where Mesa and the specification text disagree about
   *which* error to raise. glGetProgramInterfaceiv with an unrecognised
   interface or pname reads as GL_INVALID_ENUM in the spec and Mesa answers
   GL_INVALID_OPERATION. Which of them is right is not what this probe is
   for: it is looking for the guest returning quietly where the host would
   have raised something, so "an error, any error" is the honest assertion
   and it still catches that. */
static void check_raised(const char *name, GLenum got) {
    ++cases;
    const int ok = got != GL_NO_ERROR;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-34s got=0x%04x want=any error\n", name,
            (unsigned)got);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static GLenum drain(void) {
    GLenum last = GL_NO_ERROR, e;
    while ((e = glGetError()) != GL_NO_ERROR) last = e;
    return last;
}

static const char *vs_src =
    "#version 430 core\n"
    "layout(location = 0) in vec4 pos;\n"
    "uniform float scale;\n"
    "void main() { gl_Position = pos * scale; }\n";
static const char *fs_src =
    "#version 430 core\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(1.0); }\n";

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

    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL); glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL); glCompileShader(fs);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs); glAttachShader(prog, fs);
    glLinkProgram(prog);
    GLint linked = 0; glGetProgramiv(prog, GL_LINK_STATUS, &linked);
    if (!linked) { char log[1024] = {0};
                   glGetProgramInfoLog(prog, sizeof log, NULL, log);
                   fprintf(stderr, "INFO link failed: %s\n", log);
                   printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }
    /* A program object that exists but has never been linked, and a name that
     * is a shader rather than a program: GL distinguishes them. */
    GLuint unlinked = glCreateProgram();
    drain();

    GLint value = 0;
    char name_buffer[64];
    GLsizei length = 0;
    const GLenum one_prop = GL_TYPE;

    /* glGetProgramInterfaceiv */
    glGetProgramInterfaceiv(prog, GL_UNIFORM, GL_ACTIVE_RESOURCES, &value);
    check("interfaceiv_valid_is_clean", drain(), GL_NO_ERROR);
    glGetProgramInterfaceiv(prog, GL_TEXTURE_2D /* not an interface */,
                            GL_ACTIVE_RESOURCES, &value);
    check_raised("interfaceiv_bad_interface", drain());
    glGetProgramInterfaceiv(prog, GL_UNIFORM, GL_TEXTURE_2D /* not a pname */,
                            &value);
    check_raised("interfaceiv_bad_pname", drain());
    glGetProgramInterfaceiv(vs /* a shader, not a program */, GL_UNIFORM,
                            GL_ACTIVE_RESOURCES, &value);
    check("interfaceiv_shader_name", drain(), GL_INVALID_OPERATION);
    /* GL_MAX_NAME_LENGTH is meaningless for the two buffer interfaces. */
    glGetProgramInterfaceiv(prog, GL_ATOMIC_COUNTER_BUFFER,
                            GL_MAX_NAME_LENGTH, &value);
    check("interfaceiv_buffer_name_length", drain(), GL_INVALID_OPERATION);

    /* glGetProgramResourceIndex */
    (void)glGetProgramResourceIndex(prog, GL_UNIFORM, "scale");
    check("resourceindex_valid_is_clean", drain(), GL_NO_ERROR);
    (void)glGetProgramResourceIndex(prog, GL_TEXTURE_2D, "scale");
    check("resourceindex_bad_interface", drain(), GL_INVALID_ENUM);
    /* The buffer interfaces have no names to look up. */
    (void)glGetProgramResourceIndex(prog, GL_ATOMIC_COUNTER_BUFFER, "scale");
    check("resourceindex_buffer_interface", drain(), GL_INVALID_ENUM);
    (void)glGetProgramResourceIndex(vs, GL_UNIFORM, "scale");
    check("resourceindex_shader_name", drain(), GL_INVALID_OPERATION);

    /* glGetProgramResourceName */
    glGetProgramResourceName(prog, GL_UNIFORM, 0, sizeof name_buffer,
                             &length, name_buffer);
    check("resourcename_valid_is_clean", drain(), GL_NO_ERROR);
    glGetProgramResourceName(prog, GL_UNIFORM, 4096 /* no such index */,
                             sizeof name_buffer, &length, name_buffer);
    check("resourcename_bad_index", drain(), GL_INVALID_VALUE);
    glGetProgramResourceName(prog, GL_ATOMIC_COUNTER_BUFFER, 0,
                             sizeof name_buffer, &length, name_buffer);
    check("resourcename_buffer_interface", drain(), GL_INVALID_ENUM);

    /* glGetProgramResourceiv */
    glGetProgramResourceiv(prog, GL_UNIFORM, 0, 1, &one_prop, 1, &length,
                           &value);
    check("resourceiv_valid_is_clean", drain(), GL_NO_ERROR);
    glGetProgramResourceiv(prog, GL_UNIFORM, 4096, 1, &one_prop, 1, &length,
                           &value);
    check("resourceiv_bad_index", drain(), GL_INVALID_VALUE);
    {
        const GLenum bad_prop = GL_TEXTURE_2D;
        glGetProgramResourceiv(prog, GL_UNIFORM, 0, 1, &bad_prop, 1, &length,
                               &value);
        check("resourceiv_bad_prop", drain(), GL_INVALID_ENUM);
    }
    glGetProgramResourceiv(prog, GL_UNIFORM, 0, 0 /* propCount */, &one_prop,
                           1, &length, &value);
    check("resourceiv_zero_propcount", drain(), GL_INVALID_VALUE);

    /* glGetProgramResourceLocation wants a *linked* program. */
    (void)glGetProgramResourceLocation(prog, GL_UNIFORM, "scale");
    check("resourcelocation_valid_is_clean", drain(), GL_NO_ERROR);
    (void)glGetProgramResourceLocation(unlinked, GL_UNIFORM, "scale");
    check("resourcelocation_unlinked", drain(), GL_INVALID_OPERATION);
    (void)glGetProgramResourceLocation(prog, GL_ATOMIC_COUNTER_BUFFER,
                                       "scale");
    check("resourcelocation_buffer_interface", drain(), GL_INVALID_ENUM);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
