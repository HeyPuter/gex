/* gl_Position and gl_PointSize across the control -> evaluation boundary.
 *
 * `tessellation_control_to_tessellation_evaluation` is the largest single
 * failing family in KHR-GL44 -- 76 of the 109 tessellation failures -- and 72
 * of those 76 fail on one of two messages: "Invalid gl_Position data exposed
 * in TE stage" or "Invalid gl_PointSize data exposed in TE stage". So the
 * whole cluster is one question: do the *builtins* of `gl_out[]` reach
 * `gl_in[]`, or only the user-named varyings beside them?
 *
 * That distinction exists here because neither stage is a real stage. Both run
 * as compute passes writing into a buffer, and in a compute shader
 * `gl_Position` is not a builtin at all -- it has to be laid out in that
 * buffer like any other varying. A layout pass that walks the shader's
 * variables picks it up; one that skips builtins, or that lays them out at a
 * slot the other stage computes differently, drops them silently. Nothing
 * refuses, because from the driver's point of view a store happened.
 *
 *   `varying`    a user-named per-vertex float through the same hop. This is
 *                the control, and it must pass for the other two to mean
 *                anything -- if it fails, the harness is wrong, not the
 *                builtin path
 *   `position`   the sum of `gl_in[n].gl_Position` over the patch
 *   `pointsize`  the sum of `gl_in[n].gl_PointSize` over the patch
 *
 * Each vertex carries a *different* value, and the evaluation shader sums
 * across the patch, so the sum identifies the failure: 0 means nothing
 * arrived, and the value for one vertex repeated N times (4 or 16 here) means
 * every `gl_in[n]` read the same vertex -- which is the mistake a per-vertex
 * stride of zero makes, and it is not distinguishable from a correct sum
 * unless the per-vertex values differ.
 *
 * The results come back through transform feedback, which is how the CTS reads
 * them, and the levels are set high enough that the patch becomes a few dozen
 * triangles rather than one -- because the sum is patch-constant, so *every*
 * captured vertex must carry it, and `every_vertex` is the case that says so.
 * A capture that records the first primitive and stops satisfies a check that
 * reads vertex 0 alone; the CTS notices because it collects unique values and
 * requires there to be exactly one.
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

/* Four vertices per patch: small enough to read by hand, and the four values
 * 1,2,3,4 sum to 10, which no single one of them equals. */
#define PATCH 4

static const char *vs_src =
    "#version 330 core\n"
    "void main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";

/* Every output is a distinct function of the invocation, so a stride mistake
 * shows up as a wrong sum rather than as a plausible one. */
static const char *tcs_src =
    "#version 330 core\n"
    "#extension GL_ARB_tessellation_shader : require\n"
    "layout(vertices = 4) out;\n"
    "out float tc_value[];\n"
    "void main() {\n"
    "  float n = float(gl_InvocationID + 1);\n"
    "  gl_out[gl_InvocationID].gl_Position = vec4(n, n * 2.0, n * 3.0, n);\n"
    "  gl_out[gl_InvocationID].gl_PointSize = n * 0.25;\n"
    "  tc_value[gl_InvocationID] = n * 8.0;\n"
    "  gl_TessLevelOuter[0] = 4.0;\n"
    "  gl_TessLevelOuter[1] = 4.0;\n"
    "  gl_TessLevelOuter[2] = 4.0;\n"
    "  gl_TessLevelOuter[3] = 4.0;\n"
    "  gl_TessLevelInner[0] = 4.0;\n"
    "  gl_TessLevelInner[1] = 4.0;\n"
    "}\n";

/* No point mode: the domain becomes triangles, which is the shape the CTS's
 * own cases use, and the loop below is the CTS's too -- it reads every gl_in[]
 * slot rather than a single one.
 */
static const char *tes_src =
    "#version 330 core\n"
    "#extension GL_ARB_tessellation_shader : require\n"
    "layout(quads, equal_spacing, ccw) in;\n"
    "in float tc_value[];\n"
    "out vec4 te_position;\n"
    "out float te_pointsize;\n"
    "out float te_value;\n"
    "void main() {\n"
    "  te_position = vec4(0.0);\n"
    "  te_pointsize = 0.0;\n"
    "  te_value = 0.0;\n"
    "  for (int n = 0; n < 4; ++n) {\n"
    "    te_position += gl_in[n].gl_Position;\n"
    "    te_pointsize += gl_in[n].gl_PointSize;\n"
    "    te_value += tc_value[n];\n"
    "  }\n"
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

    struct { GLenum stage; const char *source; const char *label; } parts[] = {
        {GL_VERTEX_SHADER, vs_src, "vertex"},
        {GL_TESS_CONTROL_SHADER, tcs_src, "control"},
        {GL_TESS_EVALUATION_SHADER, tes_src, "evaluation"},
    };
    GLuint program = glCreateProgram();
    for (unsigned i = 0; i < 3; ++i) {
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
    /* Interleaved, so one buffer holds all three and the offsets are fixed:
     * te_position at 0, te_pointsize at 4, te_value at 5 (in floats). */
    const char *captured[3] = {"te_position", "te_pointsize", "te_value"};
    glTransformFeedbackVaryings(program, 3, captured, GL_INTERLEAVED_ATTRIBS);
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

    GLuint vao = 0, xfb = 0, query = 0, gen_query = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    /* Room for far more points than a level-1 quad can make, so an
     * over-tessellating driver overflows the check rather than the buffer. */
    const unsigned floats = 6, points = 256;
    float *zeroes = calloc(floats * points, sizeof(float));
    glGenBuffers(1, &xfb);
    glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, xfb);
    glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER,
                 (GLsizeiptr)(floats * points * sizeof(float)), zeroes,
                 GL_DYNAMIC_READ);
    glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, xfb);
    glGenQueries(1, &query);
    glGenQueries(1, &gen_query);

    glUseProgram(program);
    glPatchParameteri(GL_PATCH_VERTICES, PATCH);
    glEnable(GL_RASTERIZER_DISCARD);
    glBeginQuery(GL_TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN, query);
    glBeginQuery(GL_PRIMITIVES_GENERATED, gen_query);
    glBeginTransformFeedback(GL_TRIANGLES);
    glDrawArrays(GL_PATCHES, 0, PATCH);
    glEndTransformFeedback();
    glEndQuery(GL_PRIMITIVES_GENERATED);
    glEndQuery(GL_TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN);
    glDisable(GL_RASTERIZER_DISCARD);

    GLuint written = 0, generated = 0;
    glGetQueryObjectuiv(query, GL_QUERY_RESULT, &written);
    glGetQueryObjectuiv(gen_query, GL_QUERY_RESULT, &generated);
    /* Every captured vertex, not just the first. The sum is patch-constant,
     * so all of them must carry the same value -- and the CTS checks exactly
     * this, by collecting the *unique* values it read and requiring there to
     * be one. A capture that records the first primitive correctly and leaves
     * the rest at zero passes any check that reads vertex 0 alone, which is
     * how a partial capture survives a probe that looks only at the front. */
    const unsigned vertices_read = written * 3u < points ? written * 3u : points;
    float *all = calloc(floats * points, sizeof(float));
    glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, xfb);
    glGetBufferSubData(GL_TRANSFORM_FEEDBACK_BUFFER, 0,
                       (GLsizeiptr)(floats * points * sizeof(float)), all);
    float got[6] = {0, 0, 0, 0, 0, 0};
    memcpy(got, all, sizeof(got));
    /* Which vertex first disagrees with vertex 0, and in which component --
     * a partial capture reports the primitive it stopped at. */
    int uniform_ok = vertices_read > 0;
    unsigned first_bad = 0;
    for (unsigned v = 1; v < vertices_read && uniform_ok; ++v) {
        for (unsigned c = 0; c < floats; ++c) {
            if (fabsf(all[v * floats + c] - got[c]) > 0.01f) {
                uniform_ok = 0;
                first_bad = v;
                break;
            }
        }
    }
    const GLenum error = glGetError();
    fprintf(stderr, "INFO written=%u position=%.2f,%.2f,%.2f,%.2f "
            "pointsize=%.3f value=%.2f err=0x%04x\n", written, got[0], got[1],
            got[2], got[3], got[4], got[5], (unsigned)error);

    /* Nothing captured means the draw never reached feedback, and every case
     * below would then fail for that reason rather than its own -- so say so
     * once here instead of three times. */
    if (!written)
        fprintf(stderr, "INFO no primitives were captured at all\n");

    /* 1+2+3+4 = 10, so te_value = 8 * 10 = 80. A stride of zero would give
     * 4 * 8 * k for a single k: 32, 64, 96 or 128 -- none of them 80. */
    result("varying", error == GL_NO_ERROR && written > 0 &&
                      fabsf(got[5] - 80.0f) < 0.01f);
    /* The same sum in each channel: (10, 20, 30, 10). */
    result("position", error == GL_NO_ERROR && written > 0 &&
                       fabsf(got[0] - 10.0f) < 0.01f &&
                       fabsf(got[1] - 20.0f) < 0.01f &&
                       fabsf(got[2] - 30.0f) < 0.01f &&
                       fabsf(got[3] - 10.0f) < 0.01f);
    /* 0.25 * 10 = 2.5. */
    result("pointsize", error == GL_NO_ERROR && written > 0 &&
                        fabsf(got[4] - 2.5f) < 0.01f);
    /* All 3 * written vertices carry it, not just the first. */
    if (!uniform_ok)
        fprintf(stderr, "INFO vertex %u of %u differs: %.2f,%.2f,%.2f,%.2f "
                "ps=%.3f value=%.2f\n", first_bad, vertices_read,
                all[first_bad * floats + 0], all[first_bad * floats + 1],
                all[first_bad * floats + 2], all[first_bad * floats + 3],
                all[first_bad * floats + 4], all[first_bad * floats + 5]);
    result("every_vertex", error == GL_NO_ERROR && written > 0 && uniform_ok);
    /* The buffer has room for far more than this patch produces, so nothing
     * overflowed and GL must report the same number twice. They differed by
     * exactly 2x while the tessellated draw was counted three times over:
     * once for the patch draw, whose primitive count is meaningless because
     * the tessellator has not run, and again for the pass that rasterizes
     * the finished mesh. It is worth a case of its own because applications
     * rarely read this query -- but the CTS *sizes its feedback buffer* from
     * it, so a wrong count there turns into a half-filled buffer and a wrong
     * answer somewhere else entirely. */
    fprintf(stderr, "INFO generated=%u written=%u\n", generated, written);
    result("primitives_generated", error == GL_NO_ERROR && generated > 0 &&
                                   generated == written);

    free(all);
    free(zeroes);
    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
