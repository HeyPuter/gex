/* Multiple geometry-shader output streams -- ARB_transform_feedback3, core
 * GL 4.0.
 *
 * A geometry shader may route its output to up to `GL_MAX_VERTEX_STREAMS`
 * separate streams with `EmitStreamVertex(n)`, each stream feeding its own
 * transform-feedback buffer. Only stream zero is rasterized; the others exist
 * purely to be captured, which is how a single pass both draws geometry and
 * sorts derived data into separate buckets.
 *
 * GL requires the limit to be at least 4. This driver does not set the
 * Gallium cap at all, so `st` clamps it to `MAX2(1, 0)` = 1, and the geometry
 * path refuses any shader whose `active_stream_mask` has a bit above zero:
 *
 *     gex_wgpu geometry shaders may only write stream zero
 *
 * A refusal is the honest failure, and it is what these cases meet today.
 * The reason to write them before the implementation is the *second* failure
 * mode, which a limit check alone cannot see: a driver that accepts the
 * shader and then routes every stream into stream zero's buffer. That looks
 * like success from the draw's side -- geometry appears, feedback buffers
 * fill -- and is wrong in a way only the per-stream contents show.
 *
 *   streams_advertised  GL_MAX_VERTEX_STREAMS >= 4, which GL 4.0 requires of
 *                       any implementation claiming the version. A control:
 *                       if this fails the rest cannot be expected to pass.
 *   two_streams         streams 0 and 1 into two buffers, with values that
 *                       say which stream wrote them. Both buffers start at a
 *                       sentinel, so "stream 1 never ran" and "stream 1 wrote
 *                       stream 0's values" are distinct failures.
 *   stream_one_only     a shader that emits on stream 1 alone. Buffer 0 must
 *                       stay at its sentinel. This is the case that catches
 *                       routing every stream to zero, because the driver that
 *                       does it fills the wrong buffer and leaves the right
 *                       one empty.
 *   four_streams        all four at once, one buffer each. A driver that
 *                       special-cased two streams stops here, and so does one
 *                       whose advertised limit is larger than what it serves.
 *   generated_stream1   GL_PRIMITIVES_GENERATED, queried per stream with
 *                       glBeginQueryIndexed. The two streams emit *different*
 *                       counts -- 1 and 2 primitives per invocation -- so a
 *                       driver answering the wrong stream's total, or
 *                       answering zero as this one does for any index above
 *                       0, is caught rather than coincidentally right.
 *   written_stream1     the same for TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN,
 *                       which is the count GL promises equals what landed in
 *                       stream 1's buffer.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifndef GL_MAX_VERTEX_STREAMS
#define GL_MAX_VERTEX_STREAMS 0x8E71
#endif

#define SIDE 8
#define POINTS 3          /* geometry invocations */
#define SLOTS 64          /* floats reserved per feedback buffer */
#define SENTINEL (-7.5f)  /* no case ever writes this */

static int cases;
static int passed;
static int advertised;

static void (*begin_query_indexed)(GLenum, GLuint, GLuint);
static void (*end_query_indexed)(GLenum, GLuint);

static void result(const char *name, int ok, const char *detail)
{
    ++cases;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-18s %s\n", name, detail);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 400 core\n"
    "void main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
static const char *fs_src =
    "#version 400 core\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(1.0); }\n";

/* Link a geometry program capturing `varyings` into separate buffers. Returns
 * 0 and fills `log` when the driver refuses -- which is the expected result
 * until multi-stream lands, and is reported rather than hidden. */
static GLuint build(const char *gs_src, const char **varyings, int nvaryings,
                    char *log, size_t logsz)
{
    GLuint v = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(v, 1, &vs_src, NULL); glCompileShader(v);
    GLuint g = glCreateShader(GL_GEOMETRY_SHADER);
    glShaderSource(g, 1, &gs_src, NULL); glCompileShader(g);
    GLint ok = 0;
    glGetShaderiv(g, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        glGetShaderInfoLog(g, (GLsizei)logsz - 1, NULL, log);
        return 0;
    }
    GLuint f = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(f, 1, &fs_src, NULL); glCompileShader(f);

    GLuint p = glCreateProgram();
    glAttachShader(p, v); glAttachShader(p, g); glAttachShader(p, f);
    glTransformFeedbackVaryings(p, nvaryings, varyings, GL_INTERLEAVED_ATTRIBS);
    glLinkProgram(p);
    glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) {
        glGetProgramInfoLog(p, (GLsizei)logsz - 1, NULL, log);
        return 0;
    }
    return p;
}

/* Reset every feedback buffer to the sentinel, so an untouched buffer is
 * visibly untouched rather than holding a plausible zero. */
static void arm(GLuint *bufs, int n)
{
    float fill[SLOTS];
    for (int i = 0; i < SLOTS; ++i) fill[i] = SENTINEL;
    for (int i = 0; i < n; ++i) {
        glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, bufs[i]);
        glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER, sizeof fill, fill,
                     GL_DYNAMIC_READ);
        glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, (GLuint)i, bufs[i]);
    }
}

static void readback(GLuint buf, float *out, int n)
{
    glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, buf);
    glGetBufferSubData(GL_TRANSFORM_FEEDBACK_BUFFER, 0,
                       (GLsizeiptr)(n * sizeof(float)), out);
}

/* Does `got` hold `n` values, in any order, matching `want`? Order within a
 * stream is guaranteed by GL, but the shaders here emit one primitive per
 * invocation per stream and invocation order is what the draw fixes, so an
 * exact compare is right -- this stays ordered deliberately. */
static int same(const float *got, const float *want, int n)
{
    for (int i = 0; i < n; ++i)
        if (got[i] < want[i] - 0.01f || got[i] > want[i] + 0.01f) return 0;
    return 1;
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(d, NULL, NULL)) { printf("FATAL eglInitialize\n"); return 2; }
    static const EGLint ca[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                                EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_NONE};
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    eglBindAPI(EGL_OPENGL_API);
    static const EGLint xa[] = {EGL_CONTEXT_MAJOR_VERSION, 4,
                                EGL_CONTEXT_MINOR_VERSION, 0, EGL_NONE};
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, xa);
    if (x == EGL_NO_CONTEXT) { printf("FATAL eglCreateContext\n"); return 2; }
    static const EGLint sa[] = {EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE, EGL_NONE};
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!eglMakeCurrent(d, s, s, x)) { printf("FATAL eglMakeCurrent\n"); return 2; }
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    /* Indexed queries through the loader: this non-glvnd libGL does not
     * export every GL 4.0 entry point, and a missing symbol kills the process
     * at load rather than failing a case. */
    begin_query_indexed = (void (*)(GLenum, GLuint, GLuint))
        eglGetProcAddress("glBeginQueryIndexed");
    end_query_indexed = (void (*)(GLenum, GLuint))
        eglGetProcAddress("glEndQueryIndexed");

    GLint streams = 0;
    glGetIntegerv(GL_MAX_VERTEX_STREAMS, &streams);
    (void)glGetError();
    advertised = streams >= 4;
    {
        char detail[96];
        snprintf(detail, sizeof(detail), "GL_MAX_VERTEX_STREAMS=%d (GL 4.0 wants >= 4)",
                 streams);
        result("streams_advertised", advertised, detail);
    }

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    GLuint bufs[4]; glGenBuffers(4, bufs);
    glEnable(GL_RASTERIZER_DISCARD);

    char log[600];
    float got[SLOTS];

    /* ---- two_streams ------------------------------------------------- */
    /* Stream 0 emits one point per invocation, stream 1 emits two. The
     * values name both the stream and the invocation, so a buffer that is
     * right by accident is not reachable. */
    {
        static const char *src =
            "#version 400 core\n"
            "layout(points) in;\n"
            "layout(points, max_vertices = 8) out;\n"
            "layout(stream = 0) out float a;\n"
            "layout(stream = 1) out float b;\n"
            "void main() {\n"
            "  float id = float(gl_PrimitiveIDIn);\n"
            "  a = 100.0 + id; EmitStreamVertex(0); EndStreamPrimitive(0);\n"
            "  b = 200.0 + id; EmitStreamVertex(1); EndStreamPrimitive(1);\n"
            "  b = 300.0 + id; EmitStreamVertex(1); EndStreamPrimitive(1);\n"
            "}\n";
        static const char *vary[] = {"a", "gl_NextBuffer", "b"};
        memset(log, 0, sizeof log);
        GLuint p = build(src, vary, 3, log, sizeof log);
        if (!p) {
            result("two_streams", 0, log[0] ? log : "link failed");
        } else {
            arm(bufs, 2);
            glUseProgram(p);
            glBeginTransformFeedback(GL_POINTS);
            glDrawArrays(GL_POINTS, 0, POINTS);
            glEndTransformFeedback();
            glFinish();
            static const float want0[POINTS] = {100.0f, 101.0f, 102.0f};
            static const float want1[POINTS * 2] = {200.0f, 300.0f, 201.0f,
                                                    301.0f, 202.0f, 302.0f};
            readback(bufs[0], got, POINTS);
            const int ok0 = same(got, want0, POINTS);
            float got1[POINTS * 2];
            readback(bufs[1], got1, POINTS * 2);
            const int ok1 = same(got1, want1, POINTS * 2);
            char detail[220];
            snprintf(detail, sizeof(detail),
                     "buf0=%.0f,%.0f,%.0f buf1=%.0f,%.0f,%.0f,%.0f,%.0f,%.0f",
                     (double)got[0], (double)got[1], (double)got[2],
                     (double)got1[0], (double)got1[1], (double)got1[2],
                     (double)got1[3], (double)got1[4], (double)got1[5]);
            result("two_streams", ok0 && ok1, detail);
            glDeleteProgram(p);
        }
    }

    /* ---- stream_one_only --------------------------------------------- */
    /* Nothing is emitted on stream 0, so buffer 0 must come back holding the
     * sentinel. A driver that routes every stream into stream zero fills it
     * instead, which is the silent failure this case exists for. */
    {
        static const char *src =
            "#version 400 core\n"
            "layout(points) in;\n"
            "layout(points, max_vertices = 8) out;\n"
            "layout(stream = 0) out float a;\n"
            "layout(stream = 1) out float b;\n"
            "void main() {\n"
            "  b = 500.0 + float(gl_PrimitiveIDIn);\n"
            "  EmitStreamVertex(1); EndStreamPrimitive(1);\n"
            "}\n";
        static const char *vary[] = {"a", "gl_NextBuffer", "b"};
        memset(log, 0, sizeof log);
        GLuint p = build(src, vary, 3, log, sizeof log);
        if (!p) {
            result("stream_one_only", 0, log[0] ? log : "link failed");
        } else {
            arm(bufs, 2);
            glUseProgram(p);
            glBeginTransformFeedback(GL_POINTS);
            glDrawArrays(GL_POINTS, 0, POINTS);
            glEndTransformFeedback();
            glFinish();
            readback(bufs[0], got, 1);
            float got1[POINTS];
            readback(bufs[1], got1, POINTS);
            static const float want1[POINTS] = {500.0f, 501.0f, 502.0f};
            const int untouched = got[0] < SENTINEL + 0.01f &&
                                  got[0] > SENTINEL - 0.01f;
            char detail[200];
            snprintf(detail, sizeof(detail),
                     "buf0[0]=%.1f (want sentinel %.1f) buf1=%.0f,%.0f,%.0f",
                     (double)got[0], (double)SENTINEL, (double)got1[0],
                     (double)got1[1], (double)got1[2]);
            result("stream_one_only", untouched && same(got1, want1, POINTS),
                   detail);
            glDeleteProgram(p);
        }
    }

    /* ---- four_streams ------------------------------------------------ */
    {
        static const char *src =
            "#version 400 core\n"
            "layout(points) in;\n"
            "layout(points, max_vertices = 8) out;\n"
            "layout(stream = 0) out float a;\n"
            "layout(stream = 1) out float b;\n"
            "layout(stream = 2) out float c;\n"
            "layout(stream = 3) out float e;\n"
            "void main() {\n"
            "  float id = float(gl_PrimitiveIDIn);\n"
            "  a = 1000.0 + id; EmitStreamVertex(0); EndStreamPrimitive(0);\n"
            "  b = 2000.0 + id; EmitStreamVertex(1); EndStreamPrimitive(1);\n"
            "  c = 3000.0 + id; EmitStreamVertex(2); EndStreamPrimitive(2);\n"
            "  e = 4000.0 + id; EmitStreamVertex(3); EndStreamPrimitive(3);\n"
            "}\n";
        static const char *vary[] = {"a", "gl_NextBuffer", "b", "gl_NextBuffer",
                                     "c", "gl_NextBuffer", "e"};
        memset(log, 0, sizeof log);
        GLuint p = build(src, vary, 7, log, sizeof log);
        if (!p) {
            result("four_streams", 0, log[0] ? log : "link failed");
        } else {
            arm(bufs, 4);
            glUseProgram(p);
            glBeginTransformFeedback(GL_POINTS);
            glDrawArrays(GL_POINTS, 0, POINTS);
            glEndTransformFeedback();
            glFinish();
            int ok = 1;
            char detail[260];
            int at = 0;
            for (int st = 0; st < 4; ++st) {
                float want[POINTS];
                for (int i = 0; i < POINTS; ++i)
                    want[i] = (float)(1000 * (st + 1) + i);
                readback(bufs[st], got, POINTS);
                if (!same(got, want, POINTS)) ok = 0;
                at += snprintf(detail + at, sizeof(detail) - (size_t)at,
                               "s%d=%.0f,%.0f,%.0f ", st, (double)got[0],
                               (double)got[1], (double)got[2]);
                if (at >= (int)sizeof(detail)) break;
            }
            result("four_streams", ok, detail);
            glDeleteProgram(p);
        }
    }

    /* ---- generated_stream1 / written_stream1 -------------------------- */
    /* One shader, two queries. Stream 0 closes one primitive per invocation
     * and stream 1 closes two, so the totals are 3 and 6: a driver that
     * answers the other stream's count, or answers zero for any index above
     * zero as this one does today, cannot land on the right number. */
    if (!begin_query_indexed || !end_query_indexed) {
        result("generated_stream1", 0, "no glBeginQueryIndexed entry point");
        result("written_stream1", 0, "no glBeginQueryIndexed entry point");
    } else {
        static const char *src =
            "#version 400 core\n"
            "layout(points) in;\n"
            "layout(points, max_vertices = 8) out;\n"
            "layout(stream = 0) out float a;\n"
            "layout(stream = 1) out float b;\n"
            "void main() {\n"
            "  float id = float(gl_PrimitiveIDIn);\n"
            "  a = 100.0 + id; EmitStreamVertex(0); EndStreamPrimitive(0);\n"
            "  b = 200.0 + id; EmitStreamVertex(1); EndStreamPrimitive(1);\n"
            "  b = 300.0 + id; EmitStreamVertex(1); EndStreamPrimitive(1);\n"
            "}\n";
        static const char *vary[] = {"a", "gl_NextBuffer", "b"};
        memset(log, 0, sizeof log);
        GLuint p = build(src, vary, 3, log, sizeof log);
        if (!p) {
            result("generated_stream1", 0, log[0] ? log : "link failed");
            result("written_stream1", 0, log[0] ? log : "link failed");
        } else {
            GLuint q[2]; glGenQueries(2, q);
            arm(bufs, 2);
            glUseProgram(p);
            begin_query_indexed(GL_PRIMITIVES_GENERATED, 1, q[0]);
            begin_query_indexed(GL_TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN, 1,
                                q[1]);
            glBeginTransformFeedback(GL_POINTS);
            glDrawArrays(GL_POINTS, 0, POINTS);
            glEndTransformFeedback();
            end_query_indexed(GL_TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN, 1);
            end_query_indexed(GL_PRIMITIVES_GENERATED, 1);
            glFinish();
            GLuint generated = 0xffff, written = 0xffff;
            glGetQueryObjectuiv(q[0], GL_QUERY_RESULT, &generated);
            glGetQueryObjectuiv(q[1], GL_QUERY_RESULT, &written);
            const GLenum err = glGetError();
            char detail[180];
            snprintf(detail, sizeof(detail),
                     "stream1 generated=%u want=%u (stream0 would be %u) err=0x%04x",
                     generated, (unsigned)(POINTS * 2), (unsigned)POINTS,
                     (unsigned)err);
            result("generated_stream1", generated == POINTS * 2, detail);
            snprintf(detail, sizeof(detail),
                     "stream1 written=%u want=%u err=0x%04x", written,
                     (unsigned)(POINTS * 2), (unsigned)err);
            result("written_stream1", written == POINTS * 2, detail);
            glDeleteQueries(2, q);
            glDeleteProgram(p);
        }
    }

    glDisable(GL_RASTERIZER_DISCARD);
    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
