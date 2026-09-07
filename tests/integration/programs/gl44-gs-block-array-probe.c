/* A uniform block array, with an explicit binding, read from a geometry
 * shader.
 *
 * gl44-block-array-binding-probe showed the binding *rule* is right in a
 * vertex shader -- layout(binding = 2) on a three-element array gives 2, 3, 4
 * -- and 5/5 on both paths. So that is not why
 * KHR-GL44.shading_language_420pack.binding_uniform_block_array fails in the
 * browser. That case is a GLSLTestBase and runs its shader through every
 * stage, and this driver emulates geometry and tessellation as compute, so the
 * remaining suspect is a block array inside an emulated stage.
 *
 * Two questions, because they fail differently: are the bindings still
 * assigned correctly when a geometry shader is in the program, and does the
 * data actually arrive there. A binding that is right while the buffer behind
 * it is not read is the shape this driver's emulated stages have had before --
 * an advertised resource the stage cannot really reach.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;

static GLenum drain(void) {
    GLenum l = GL_NO_ERROR, e;
    while ((e = glGetError()) != GL_NO_ERROR) l = e;
    return l;
}

static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-38s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 420 core\n"
    "void main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
/* The array is read in the geometry stage, and its three elements carry
   distinct values so a mix-up between them shows as a wrong number rather
   than as nothing. */
static const char *gs_src =
    "#version 420 core\n"
    "layout(points) in;\n"
    "layout(triangle_strip, max_vertices = 4) out;\n"
    "layout(std140, binding = 2) uniform GOKU {\n"
    "    vec4 value;\n"
    "} goku[3];\n"
    "out float carried;\n"
    "void main() {\n"
    "    carried = goku[0].value.x + goku[1].value.x * 2.0\n"
    "            + goku[2].value.x * 4.0;\n"
    "    gl_Position = vec4(-1.0, -1.0, 0.0, 1.0); EmitVertex();\n"
    "    gl_Position = vec4( 1.0, -1.0, 0.0, 1.0); EmitVertex();\n"
    "    gl_Position = vec4(-1.0,  1.0, 0.0, 1.0); EmitVertex();\n"
    "    gl_Position = vec4( 1.0,  1.0, 0.0, 1.0); EmitVertex();\n"
    "    EndPrimitive();\n"
    "}\n";
static const char *fs_src =
    "#version 420 core\n"
    "in float carried;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(carried / 255.0, 0.0, 0.0, 1.0); }\n";

static GLuint compile(GLenum stage, const char *src, const char *what) {
    GLuint sh = glCreateShader(stage);
    glShaderSource(sh, 1, &src, NULL); glCompileShader(sh);
    GLint ok = 0; glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[1024] = {0};
               glGetShaderInfoLog(sh, sizeof log, NULL, log);
               fprintf(stderr, "INFO %s compile failed: %s\n", what, log);
               return 0; }
    return sh;
}

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

    GLuint vs = compile(GL_VERTEX_SHADER, vs_src, "vs");
    GLuint gs = compile(GL_GEOMETRY_SHADER, gs_src, "gs");
    GLuint fs = compile(GL_FRAGMENT_SHADER, fs_src, "fs");
    if (!vs || !gs || !fs) { printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }
    GLuint program = glCreateProgram();
    glAttachShader(program, vs); glAttachShader(program, gs);
    glAttachShader(program, fs);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) { char log[1024] = {0};
                   glGetProgramInfoLog(program, sizeof log, NULL, log);
                   fprintf(stderr, "INFO link failed: %s\n", log);
                   printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }
    ++cases; ++passed;
    printf("CASE gs_block_array_links PASS\n");

    /* The bindings, with a geometry shader in the program. */
    static const char *names[3] = { "GOKU[0]", "GOKU[1]", "GOKU[2]" };
    static const char *labels[3] = { "gs_goku0_binding_is_2",
                                     "gs_goku1_binding_is_3",
                                     "gs_goku2_binding_is_4" };
    for (unsigned i = 0; i < 3; ++i) {
        const GLuint index = glGetUniformBlockIndex(program, names[i]);
        GLint binding = -1;
        if (index != GL_INVALID_INDEX)
            glGetActiveUniformBlockiv(program, index, GL_UNIFORM_BLOCK_BINDING,
                                      &binding);
        eq(labels[i], binding, 2 + i);
    }

    /* And the data. 1 + 2*2 + 3*4 = 17, so a red channel of 17 means all
       three blocks were read and read in the right order; 0 means the stage
       could not reach them, and anything else names which one went wrong. */
    GLuint ubo[3] = {0, 0, 0};
    glGenBuffers(3, ubo);
    const float values[3] = { 1.0f, 2.0f, 3.0f };
    for (unsigned i = 0; i < 3; ++i) {
        float block[4] = { values[i], 0.0f, 0.0f, 0.0f };
        glBindBuffer(GL_UNIFORM_BUFFER, ubo[i]);
        glBufferData(GL_UNIFORM_BUFFER, sizeof block, block, GL_STATIC_DRAW);
        glBindBufferBase(GL_UNIFORM_BUFFER, 2 + i, ubo[i]);
    }
    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    glUseProgram(program);
    glViewport(0, 0, 16, 16);
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f); glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_POINTS, 0, 1);
    eq("gs_draw_is_clean", drain(), GL_NO_ERROR);
    unsigned char pixel[4] = {0};
    glReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    eq("gs_read_all_three_blocks", pixel[0], 17);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
