/* Transform feedback out of a geometry shader.
 *
 * GL records the last pre-rasterization stage, so with a geometry shader
 * bound it is the geometry stage's output that is captured -- not the vertex
 * stage's. This driver used to capture nothing at all in that case and say so
 * only on stderr, leaving the feedback buffers holding whatever they held.
 *
 * Capturing it is not the same shape of problem as capturing a vertex stage.
 * A vertex shader emits exactly one vertex per invocation, so where each one
 * writes is known before the draw; a geometry shader emits a number it
 * decides at run time, and may emit none. The driver therefore records how
 * many primitives each invocation closed, sums those, and only then compacts
 * the vertices into the buffer -- and every case here is aimed at a way that
 * can go wrong:
 *
 *   `packed`     invocations emit *different* numbers of primitives, so a
 *                driver that reserved a fixed span per invocation leaves gaps
 *                and a driver that used an atomic counter loses the order
 *   `order`      the values come back in primitive order, which is what GL
 *                promises and what an out-of-order allocation breaks
 *   `strip`      a triangle strip is recorded as separate triangles: three
 *                vertices each, with the strip's own winding applied
 *   `unemitted`  an invocation that emits nothing contributes nothing, which
 *                a driver keying off the index buffer alone cannot tell from
 *                a degenerate primitive that *was* emitted
 *   `generated`  GL_PRIMITIVES_GENERATED counts what the geometry stage
 *                produced, which is a number only the GPU knows
 *
 * Every value captured is distinct and derived from gl_PrimitiveIDIn, so a
 * buffer that is right by luck is not possible.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
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

static GLuint build(const char *gs_src, const char *varying) {
    static const char *vs_src =
        "#version 330 core\n"
        "void main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
    static const char *fs_src =
        "#version 330 core\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(1.0); }\n";
    struct { GLenum stage; const char *source; const char *label; } parts[] = {
        {GL_VERTEX_SHADER, vs_src, "vertex"},
        {GL_GEOMETRY_SHADER, gs_src, "geometry"},
        {GL_FRAGMENT_SHADER, fs_src, "fragment"},
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
            return 0;
        }
        glAttachShader(program, shader);
    }
    glTransformFeedbackVaryings(program, 1, &varying, GL_INTERLEAVED_ATTRIBS);
    glLinkProgram(program);
    GLint ok = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return program;
}

/* Draw `points` points with rasterization off, and read back what the
 * feedback buffer holds. Returns the number of floats the buffer has room
 * for, filled from the start; `query` is asked for while the draw runs. */
static int capture(GLuint program, GLuint feedback, GLenum mode, int points,
                   float *out, int room, GLenum query, GLuint *query_result) {
    glUseProgram(program);
    glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, feedback);
    /* A known filler, so a slot the driver never wrote is visible as such
     * rather than reading as a plausible zero. */
    float *filler = (float *)malloc((size_t)room * sizeof(float));
    if (!filler) return 0;
    for (int i = 0; i < room; ++i) filler[i] = -999.0f;
    glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER, (GLsizeiptr)room * sizeof(float),
                 filler, GL_DYNAMIC_READ);
    free(filler);

    GLuint id = 0;
    if (query) { glGenQueries(1, &id); glBeginQuery(query, id); }
    glEnable(GL_RASTERIZER_DISCARD);
    glBeginTransformFeedback(mode);
    glDrawArrays(GL_POINTS, 0, points);
    glEndTransformFeedback();
    glDisable(GL_RASTERIZER_DISCARD);
    if (query) {
        glEndQuery(query);
        glGetQueryObjectuiv(id, GL_QUERY_RESULT, query_result);
        glDeleteQueries(1, &id);
    }
    glGetBufferSubData(GL_TRANSFORM_FEEDBACK_BUFFER, 0,
                       (GLsizeiptr)room * sizeof(float), out);
    return room;
}

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
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    GLuint vao = 0, feedback = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &feedback);
    glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, feedback);

    /* Invocation n emits n + 1 points, each carrying a value that names both
     * the invocation and which point of it this is. Three invocations
     * therefore emit 1 + 2 + 3 = 6 points, and no two carry the same value. */
    GLuint uneven = build(
        "#version 330 core\n"
        "layout(points) in;\n"
        "layout(points, max_vertices = 3) out;\n"
        "out float tag;\n"
        "void main() {\n"
        "  for (int i = 0; i <= gl_PrimitiveIDIn; ++i) {\n"
        "    tag = float(gl_PrimitiveIDIn * 10 + i);\n"
        "    gl_Position = vec4(0.0, 0.0, 0.0, 1.0);\n"
        "    EmitVertex();\n"
        "    EndPrimitive();\n"
        "  }\n"
        "}\n", "tag");
    float got[16];
    GLuint generated = 0;
    if (uneven) {
        memset(got, 0, sizeof(got));
        capture(uneven, feedback, GL_POINTS, 3, got, 8,
                GL_PRIMITIVES_GENERATED, &generated);
    }
    GLenum error = glGetError();
    /* 0, 10, 11, 20, 21, 22 -- packed, with nothing between them, and the
     * two slots past the end still holding the filler. */
    static const float want[6] = {0.0f, 10.0f, 11.0f, 20.0f, 21.0f, 22.0f};
    int packed = uneven && error == GL_NO_ERROR;
    for (int i = 0; i < 6 && packed; ++i)
        if (got[i] != want[i]) packed = 0;
    fprintf(stderr, "INFO packed got=%.0f,%.0f,%.0f,%.0f,%.0f,%.0f "
            "tail=%.0f err=0x%04x\n", got[0], got[1], got[2], got[3], got[4],
            got[5], got[6], (unsigned)error);
    result("packed", packed);
    /* The same data read as a question about ordering: invocation 2's values
     * must follow invocation 1's, which an atomic allocation does not
     * guarantee even when the set of values is right. */
    result("order", uneven && error == GL_NO_ERROR &&
                    got[0] < got[1] && got[1] < got[2] &&
                    got[2] < got[3] && got[3] < got[4] && got[4] < got[5]);
    fprintf(stderr, "INFO generated=%u want=6\n", generated);
    result("generated", uneven && error == GL_NO_ERROR && generated == 6);

    /* A four-vertex triangle strip is two triangles, and feedback records
     * them separately: six vertices, not four. */
    GLuint strip = build(
        "#version 330 core\n"
        "layout(points) in;\n"
        "layout(triangle_strip, max_vertices = 4) out;\n"
        "out float tag;\n"
        "void main() {\n"
        "  for (int i = 0; i < 4; ++i) {\n"
        "    tag = float(i);\n"
        "    gl_Position = vec4(0.0, 0.0, 0.0, 1.0);\n"
        "    EmitVertex();\n"
        "  }\n"
        "  EndPrimitive();\n"
        "}\n", "tag");
    if (strip) {
        memset(got, 0, sizeof(got));
        capture(strip, feedback, GL_TRIANGLES, 1, got, 8, 0, NULL);
    }
    error = glGetError();
    /* Vertices 0,1,2 then the second triangle, whose winding GL keeps by
     * exchanging two of its vertices -- so the tags are 0,1,2 and then some
     * arrangement of 1,2,3 rather than 1,2,3 in order. Six written, and the
     * seventh slot untouched. */
    const int six = strip && error == GL_NO_ERROR &&
                    got[0] == 0.0f && got[1] == 1.0f && got[2] == 2.0f &&
                    got[3] + got[4] + got[5] == 6.0f &&
                    got[6] == -999.0f;
    fprintf(stderr, "INFO strip got=%.0f,%.0f,%.0f,%.0f,%.0f,%.0f tail=%.0f "
            "err=0x%04x\n", got[0], got[1], got[2], got[3], got[4], got[5],
            got[6], (unsigned)error);
    result("strip", six);

    /* Every other invocation emits nothing at all. Four invocations, two of
     * which record one point each. */
    GLuint sparse = build(
        "#version 330 core\n"
        "layout(points) in;\n"
        "layout(points, max_vertices = 1) out;\n"
        "out float tag;\n"
        "void main() {\n"
        "  if ((gl_PrimitiveIDIn & 1) == 1) return;\n"
        "  tag = float(gl_PrimitiveIDIn + 1);\n"
        "  gl_Position = vec4(0.0, 0.0, 0.0, 1.0);\n"
        "  EmitVertex();\n"
        "  EndPrimitive();\n"
        "}\n", "tag");
    if (sparse) {
        memset(got, 0, sizeof(got));
        capture(sparse, feedback, GL_POINTS, 4, got, 4, 0, NULL);
    }
    error = glGetError();
    fprintf(stderr, "INFO unemitted got=%.0f,%.0f tail=%.0f err=0x%04x\n",
            got[0], got[1], got[2], (unsigned)error);
    result("unemitted", sparse && error == GL_NO_ERROR &&
                        got[0] == 1.0f && got[1] == 3.0f &&
                        got[2] == -999.0f);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
