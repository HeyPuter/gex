/* An explicit binding on an array of uniform blocks.
 *
 * GLSL 4.20's layout(binding = N) on a block *array* assigns N to its first
 * element and N+1, N+2 ... to the rest. That is the whole of what
 * KHR-GL44.shading_language_420pack.binding_uniform_block_array checks, and
 * that case fails in the browser and passes natively while logging nothing --
 * so the QPA log carries no reason and a probe is the only way to see which
 * part is wrong.
 *
 * Asked one element at a time, because "the array got the wrong bindings" and
 * "the array got no bindings" are different defects and the case cannot tell
 * them apart.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;

static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-38s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 420 core\n"
    "layout(std140, binding = 2) uniform GOKU {\n"
    "    vec4 chichi;\n"
    "} goku[3];\n"
    "layout(std140, binding = 6) uniform VEGETA {\n"
    "    vec4 bulma;\n"
    "} vegeta;\n"
    "void main() {\n"
    "    gl_Position = goku[0].chichi + goku[1].chichi + goku[2].chichi\n"
    "                + vegeta.bulma;\n"
    "}\n";
static const char *fs_src =
    "#version 420 core\n"
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
    GLint ok = 0; glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[1024] = {0};
               glGetShaderInfoLog(vs, sizeof log, NULL, log);
               fprintf(stderr, "INFO vs compile failed: %s\n", log);
               printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL); glCompileShader(fs);
    GLuint program = glCreateProgram();
    glAttachShader(program, vs); glAttachShader(program, fs);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) { char log[1024] = {0};
                   glGetProgramInfoLog(program, sizeof log, NULL, log);
                   fprintf(stderr, "INFO link failed: %s\n", log);
                   printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }
    ++cases; ++passed;
    printf("CASE block_array_links PASS\n");

    /* Each element by name, so a wrong index is named rather than averaged
       into one verdict. */
    static const char *names[3] = { "GOKU[0]", "GOKU[1]", "GOKU[2]" };
    static const char *labels[3] = { "goku0_binding_is_2", "goku1_binding_is_3",
                                     "goku2_binding_is_4" };
    for (unsigned i = 0; i < 3; ++i) {
        const GLuint index = glGetUniformBlockIndex(program, names[i]);
        if (index == GL_INVALID_INDEX) {
            ++cases;
            fprintf(stderr, "INFO %-38s no such block\n", labels[i]);
            printf("CASE %s FAIL\n", labels[i]);
            continue;
        }
        GLint binding = -1;
        glGetActiveUniformBlockiv(program, index, GL_UNIFORM_BLOCK_BINDING,
                                  &binding);
        eq(labels[i], binding, 2 + i);
    }

    /* A non-array block beside them, which must take its own binding and not
       be shifted by the array. */
    const GLuint vi = glGetUniformBlockIndex(program, "VEGETA");
    GLint vb = -1;
    if (vi != GL_INVALID_INDEX)
        glGetActiveUniformBlockiv(program, vi, GL_UNIFORM_BLOCK_BINDING, &vb);
    eq("plain_block_binding_is_6", vb, 6);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
