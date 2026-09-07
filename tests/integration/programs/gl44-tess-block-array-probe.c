/* A uniform block array, with an explicit binding, read from the tessellation
 * stages.
 *
 * The last stage of the four that
 * KHR-GL44.shading_language_420pack.binding_uniform_block_array runs its block
 * array through. Vertex is right (gl44-block-array-binding-probe), the
 * emulated geometry stage is right (gl44-gs-block-array-probe) and compute is
 * right (gl44-cs-block-array-probe), all with the data verified, so this
 * settles the stage theory either way: a failure here is the cause, and a pass
 * means the cause is not the stages at all.
 *
 * Both tessellation stages read the array, because they are emulated
 * separately here and either could be the one that cannot reach it. The
 * control stage's contribution is weighted 1, 2, 4 and the evaluation
 * stage's 8, 16, 32, so the total of 119 names which half went wrong rather
 * than merely saying the number is not 119.
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
static const char *tcs_src =
    "#version 420 core\n"
    "layout(vertices = 1) out;\n"
    "layout(std140, binding = 2) uniform GOKU {\n"
    "    vec4 value;\n"
    "} goku[3];\n"
    "out float fromControl[];\n"
    "void main() {\n"
    "    fromControl[gl_InvocationID] = goku[0].value.x + goku[1].value.x * 2.0\n"
    "                                 + goku[2].value.x * 4.0;\n"
    "    gl_out[gl_InvocationID].gl_Position = gl_in[0].gl_Position;\n"
    "    gl_TessLevelOuter[0] = 1.0; gl_TessLevelOuter[1] = 1.0;\n"
    "    gl_TessLevelOuter[2] = 1.0; gl_TessLevelOuter[3] = 1.0;\n"
    "    gl_TessLevelInner[0] = 1.0; gl_TessLevelInner[1] = 1.0;\n"
    "}\n";
static const char *tes_src =
    "#version 420 core\n"
    "layout(quads, equal_spacing, ccw) in;\n"
    "layout(std140, binding = 2) uniform GOKU {\n"
    "    vec4 value;\n"
    "} goku[3];\n"
    "in float fromControl[];\n"
    "out float carried;\n"
    "void main() {\n"
    "    const float mine = goku[0].value.x * 8.0 + goku[1].value.x * 16.0\n"
    "                     + goku[2].value.x * 32.0;\n"
    "    carried = fromControl[0] + mine;\n"
    "    gl_Position = vec4(gl_TessCoord.x * 2.0 - 1.0,\n"
    "                       gl_TessCoord.y * 2.0 - 1.0, 0.0, 1.0);\n"
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
    GLuint tcs = compile(GL_TESS_CONTROL_SHADER, tcs_src, "tcs");
    GLuint tes = compile(GL_TESS_EVALUATION_SHADER, tes_src, "tes");
    GLuint fs = compile(GL_FRAGMENT_SHADER, fs_src, "fs");
    if (!vs || !tcs || !tes || !fs) {
        printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }
    GLuint program = glCreateProgram();
    glAttachShader(program, vs); glAttachShader(program, tcs);
    glAttachShader(program, tes); glAttachShader(program, fs);
    glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) { char log[1024] = {0};
                   glGetProgramInfoLog(program, sizeof log, NULL, log);
                   fprintf(stderr, "INFO link failed: %s\n", log);
                   printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }
    ++cases; ++passed;
    printf("CASE tess_block_array_links PASS\n");

    static const char *names[3] = { "GOKU[0]", "GOKU[1]", "GOKU[2]" };
    static const char *labels[3] = { "tess_goku0_binding_is_2",
                                     "tess_goku1_binding_is_3",
                                     "tess_goku2_binding_is_4" };
    for (unsigned i = 0; i < 3; ++i) {
        const GLuint index = glGetUniformBlockIndex(program, names[i]);
        GLint binding = -1;
        if (index != GL_INVALID_INDEX)
            glGetActiveUniformBlockiv(program, index, GL_UNIFORM_BLOCK_BINDING,
                                      &binding);
        eq(labels[i], binding, 2 + i);
    }

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
    glPatchParameteri(GL_PATCH_VERTICES, 1);
    glViewport(0, 0, 16, 16);
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f); glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_PATCHES, 0, 1);
    eq("tess_draw_is_clean", drain(), GL_NO_ERROR);

    unsigned char pixel[4] = {0};
    glReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    /* 1 + 2*2 + 3*4 = 17 from the control stage, 8 + 2*16 + 3*32 = 136 from
       the evaluation stage: 153 in total. 17 alone means the evaluation stage
       read nothing, 136 alone means the control stage did. */
    eq("tess_both_stages_read_the_array", pixel[0], 153);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
