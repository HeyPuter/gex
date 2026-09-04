/* A tessellation evaluation shader with no control shader.
 *
 * GL lets the evaluation shader stand alone: bind a vertex and an evaluation
 * shader with no control shader and the patch is tessellated by the levels
 * `glPatchParameterfv` supplies, `gl_in[]` is the vertex shader's output
 * directly, and `gl_PatchVerticesIn` is the draw's `GL_PATCH_VERTICES`. It is
 * an unusual configuration to write by hand and a completely ordinary one for
 * a test suite to check, which is why it went missing here: this driver's
 * tessellated draw is five passes and the second of them is the control
 * shader, so "no control shader" meant no draw at all -- it declined by name
 * and rendered nothing.
 *
 * That silence is what made it worth a probe. Four of KHR-GL44's remaining
 * tessellation failures reported a captured value of 0 where they wanted
 * 2.7, or 4, or 3 -- which reads like a wrong builtin, and a builtin probe
 * duly reproduced none of it, because the shaders were fine and the *draw*
 * never happened. A zero in a feedback buffer means "nobody wrote here" just
 * as readily as it means "the wrong value was written", and the two need
 * different repairs.
 *
 *   `varying`         a per-vertex value from the vertex shader, summed over
 *                     the patch in the evaluation shader
 *   `position`        the same for `gl_in[n].gl_Position`
 *   `patch_vertices`  `gl_PatchVerticesIn`, which with no control shader is
 *                     the draw's patch size and not any shader's declaration
 *   `default_levels`  the primitive count the levels asked for: outer[0] and
 *                     outer[1] of (2, 3) is 2 isolines of 3 segments, so six
 *                     line primitives. A driver ignoring glPatchParameterfv
 *                     and falling back to the GL default of 1.0 produces one
 *
 * Each of the four vertices carries a different value and the shader sums
 * across the patch, so a stride of zero -- every `gl_in[n]` reading the same
 * vertex -- gives a wrong sum rather than a plausible one.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static int have_extension(const char *want) {
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    for (GLint i = 0; i < count; ++i) {
        const GLubyte *one = glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (one && strcmp((const char *)one, want) == 0) return 1;
    }
    return 0;
}

#define PATCH 4

/* 1,2,3,4 sum to 10, which no single one of them equals. */
static const char *vs_src =
    "#version 330 core\n"
    "out vec2 vs_value;\n"
    "void main() {\n"
    "  float n = float(gl_VertexID + 1);\n"
    "  gl_Position = vec4(n, n * 2.0, n * 3.0, n);\n"
    "  vs_value = vec2(n * 8.0, n * 16.0);\n"
    "}\n";

/* No point mode: isolines produce line primitives, so the primitive count is
 * outer[0] * outer[1] and `default_levels` can predict it exactly. */
static const char *tes_src =
    "#version 330 core\n"
    "#extension GL_ARB_tessellation_shader : require\n"
    "layout(isolines, equal_spacing, ccw) in;\n"
    "in vec2 vs_value[];\n"
    "out vec4 te_position;\n"
    "out vec2 te_value;\n"
    "void main() {\n"
    "  te_position = vec4(0.0);\n"
    "  te_value = vec2(0.0);\n"
    "  for (int n = 0; n < 4; ++n) {\n"
    "    te_position += gl_in[n].gl_Position;\n"
    "    te_value += vs_value[n];\n"
    "  }\n"
    "  te_position.w = float(gl_PatchVerticesIn);\n"
    "  gl_Position = vec4(0.0, 0.0, 0.0, 1.0);\n"
    "}\n";

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count)
        || config_count < 1) return 2;
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context =
        eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    const int have = have_extension("GL_ARB_tessellation_shader");
    fprintf(stderr, "INFO version=%s tessellation=%d\n",
            glGetString(GL_VERSION), have);
    if (!have) {
        fprintf(stderr, "INFO no tessellation; nothing to test\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }
    /* Mesa's non-glvnd libGL exports neither of these, and a direct call to
     * one dies at *load* -- before any output -- rather than at the call. */
    void (*patch_fv)(GLenum, const GLfloat *) =
        (void (*)(GLenum, const GLfloat *))
        eglGetProcAddress("glPatchParameterfv");
    void (*patch_i)(GLenum, GLint) =
        (void (*)(GLenum, GLint))eglGetProcAddress("glPatchParameteri");
    if (!patch_fv || !patch_i) {
        fprintf(stderr, "INFO glPatchParameter* unavailable\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }

    struct { GLenum stage; const char *source; const char *label; } parts[] = {
        {GL_VERTEX_SHADER, vs_src, "vertex"},
        {GL_TESS_EVALUATION_SHADER, tes_src, "evaluation"},
    };
    GLuint program = glCreateProgram();
    for (unsigned i = 0; i < 2; ++i) {
        GLuint shader = glCreateShader(parts[i].stage);
        glShaderSource(shader, 1, &parts[i].source, NULL);
        glCompileShader(shader);
        GLint ok = 0;
        glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
        if (!ok) {
            char log[900] = {0};
            glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO %s compile: %s\n", parts[i].label, log);
            printf("SUMMARY cases=0 passed=0 failed=0\n");
            return 1;
        }
        glAttachShader(program, shader);
    }
    /* Interleaved: te_position at float 0, te_value at float 4. */
    const char *captured[2] = {"te_position", "te_value"};
    glTransformFeedbackVaryings(program, 2, captured, GL_INTERLEAVED_ATTRIBS);
    glLinkProgram(program);
    GLint ok = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }

    const unsigned floats = 6, points = 512;
    GLuint vao = 0, xfb = 0, query = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    float *zeroes = calloc(floats * points, sizeof(float));
    glGenBuffers(1, &xfb);
    glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, xfb);
    glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER,
                 (GLsizeiptr)(floats * points * sizeof(float)), zeroes,
                 GL_DYNAMIC_READ);
    glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, xfb);
    glGenQueries(1, &query);

    glUseProgram(program);
    patch_i(GL_PATCH_VERTICES, PATCH);
    /* 2 isolines of 3 segments. The inner levels are unused by the isoline
     * domain and are set to something else entirely, so a driver reading the
     * wrong pair of words produces a different count. */
    const float outer[4] = {2.0f, 3.0f, 4.0f, 5.0f};
    const float inner[2] = {6.0f, 7.0f};
    patch_fv(GL_PATCH_DEFAULT_OUTER_LEVEL, outer);
    patch_fv(GL_PATCH_DEFAULT_INNER_LEVEL, inner);
    glEnable(GL_RASTERIZER_DISCARD);
    glBeginQuery(GL_TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN, query);
    glBeginTransformFeedback(GL_LINES);
    glDrawArrays(GL_PATCHES, 0, PATCH);
    glEndTransformFeedback();
    glEndQuery(GL_TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN);
    glDisable(GL_RASTERIZER_DISCARD);

    GLuint written = 0;
    glGetQueryObjectuiv(query, GL_QUERY_RESULT, &written);
    float got[6] = {0, 0, 0, 0, 0, 0};
    glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, xfb);
    glGetBufferSubData(GL_TRANSFORM_FEEDBACK_BUFFER, 0, sizeof(got), got);
    const GLenum error = glGetError();
    fprintf(stderr, "INFO written=%u position=%.2f,%.2f,%.2f,%.2f "
            "value=%.2f,%.2f err=0x%04x\n", written, got[0], got[1], got[2],
            got[3], got[4], got[5], (unsigned)error);
    if (!written)
        fprintf(stderr, "INFO nothing was captured, so the draw did not run\n");

    /* 8 * (1+2+3+4) = 80, and 16 * 10 = 160. */
    result("varying", error == GL_NO_ERROR && written > 0 &&
                      fabsf(got[4] - 80.0f) < 0.01f &&
                      fabsf(got[5] - 160.0f) < 0.01f);
    /* The sum in each channel: (10, 20, 30), with w replaced below. */
    result("position", error == GL_NO_ERROR && written > 0 &&
                       fabsf(got[0] - 10.0f) < 0.01f &&
                       fabsf(got[1] - 20.0f) < 0.01f &&
                       fabsf(got[2] - 30.0f) < 0.01f);
    /* Not the 32 a gl_MaxPatchVertices-sized declaration would give, nor the
     * 3 a default patch size would. */
    result("patch_vertices", error == GL_NO_ERROR && written > 0 &&
                             fabsf(got[3] - (float)PATCH) < 0.01f);
    /* outer[0] * outer[1] = 6 line segments. */
    result("default_levels", error == GL_NO_ERROR && written == 6);

    free(zeroes);
    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
