/* Geometry shader instancing: layout(invocations = N) in;
 *
 * GL 4.0 runs the geometry shader N times per input primitive, with
 * gl_InvocationID saying which run this is. This driver runs the geometry
 * stage as a compute pass, one invocation per input primitive, and refused
 * instancing outright.
 *
 * The shape of the answer is that the compute pass is already indexed by a
 * flat invocation id: dispatch prims * N of them and divide the id back into
 * the two -- the quotient picks the input primitive, the remainder *is*
 * gl_InvocationID -- while the output slice stays one per compute invocation,
 * which is why the emit path needed no change. What the cases below pin down
 * is that the division goes the right way round, because the wrong way is not
 * a crash: it draws the same number of primitives in the wrong places.
 *
 *   `four_quadrants`  one input point, four invocations, each emitting a
 *                     triangle into a different quadrant chosen by
 *                     gl_InvocationID. A driver that ran one invocation fills
 *                     one quadrant; one that fed every invocation the same id
 *                     fills the same quadrant four times; one that swapped
 *                     quotient and remainder reads the wrong input vertex.
 *   `two_primitives`  two input points at different colours, three
 *                     invocations each. Each of the six triangles must carry
 *                     its own point's colour, which is what fails if the
 *                     primitive index is the remainder rather than the
 *                     quotient.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SIDE 64

static const char *vs_src =
    "#version 330 core\n"
    "layout(location = 0) in vec2 at;\n"
    "layout(location = 1) in vec3 tint;\n"
    "out vec3 vs_tint;\n"
    "void main() { gl_Position = vec4(at, 0.0, 1.0); vs_tint = tint; }\n";

/* Each invocation emits one small triangle centred on the quadrant its
 * gl_InvocationID names. The quadrant offsets are a table so the mapping is
 * unambiguous rather than arithmetic that could accidentally agree. */
static const char *gs_src =
    "#version 330 core\n"
    "#extension GL_ARB_gpu_shader5 : require\n"
    "layout(points, invocations = 4) in;\n"
    "layout(triangle_strip, max_vertices = 3) out;\n"
    "in vec3 vs_tint[];\n"
    "out vec3 gs_tint;\n"
    "void main() {\n"
    "  vec2 centre;\n"
    "  if (gl_InvocationID == 0) centre = vec2(-0.5, -0.5);\n"
    "  else if (gl_InvocationID == 1) centre = vec2( 0.5, -0.5);\n"
    "  else if (gl_InvocationID == 2) centre = vec2(-0.5,  0.5);\n"
    "  else centre = vec2( 0.5,  0.5);\n"
    "  float h = 0.3;\n"
    "  gs_tint = vs_tint[0];\n"
    "  gl_Position = vec4(centre + vec2(-h, -h), 0.0, 1.0); EmitVertex();\n"
    "  gl_Position = vec4(centre + vec2( h, -h), 0.0, 1.0); EmitVertex();\n"
    "  gl_Position = vec4(centre + vec2(0.0,  h), 0.0, 1.0); EmitVertex();\n"
    "  EndPrimitive();\n"
    "}\n";

/* Two points, three invocations: the triangle lands in a column chosen by the
 * input point and a row chosen by gl_InvocationID, so a swap of the two is a
 * visible transpose rather than a subtle shift. */
static const char *gs2_src =
    "#version 330 core\n"
    "#extension GL_ARB_gpu_shader5 : require\n"
    "layout(points, invocations = 3) in;\n"
    "layout(triangle_strip, max_vertices = 3) out;\n"
    "in vec3 vs_tint[];\n"
    "out vec3 gs_tint;\n"
    "void main() {\n"
    "  float x = gl_in[0].gl_Position.x;\n"
    "  float y = -0.6 + 0.6 * float(gl_InvocationID);\n"
    "  float h = 0.18;\n"
    "  gs_tint = vs_tint[0];\n"
    "  gl_Position = vec4(x - h, y - h, 0.0, 1.0); EmitVertex();\n"
    "  gl_Position = vec4(x + h, y - h, 0.0, 1.0); EmitVertex();\n"
    "  gl_Position = vec4(x,     y + h, 0.0, 1.0); EmitVertex();\n"
    "  EndPrimitive();\n"
    "}\n";

static const char *fs_src =
    "#version 330 core\n"
    "in vec3 gs_tint;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(gs_tint, 1.0); }\n";

static GLuint build(const char *gs) {
    struct { GLenum stage; const char *src; const char *what; } parts[] = {
        {GL_VERTEX_SHADER, vs_src, "vs"},
        {GL_GEOMETRY_SHADER, gs, "gs"},
        {GL_FRAGMENT_SHADER, fs_src, "fs"},
    };
    GLuint prog = glCreateProgram();
    for (unsigned i = 0; i < 3; ++i) {
        GLuint sh = glCreateShader(parts[i].stage);
        if (!sh) return 0;
        glShaderSource(sh, 1, &parts[i].src, NULL);
        glCompileShader(sh);
        GLint ok = 0;
        glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
        if (!ok) {
            char log[900] = {0};
            glGetShaderInfoLog(sh, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO %s compile: %s\n", parts[i].what, log);
            glDeleteShader(sh);
            glDeleteProgram(prog);
            return 0;
        }
        glAttachShader(prog, sh);
        glDeleteShader(sh);
    }
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        glDeleteProgram(prog);
        return 0;
    }
    return prog;
}

/* Clip (x,y) to the pixel at its centre. */
static void read_at(float x, float y, unsigned char px[4]) {
    const int ix = (int)((x + 1.0f) * 0.5f * SIDE);
    const int iy = (int)((y + 1.0f) * 0.5f * SIDE);
    glReadPixels(ix, iy, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
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

    GLint max_invocations = 0;
    glGetIntegerv(GL_MAX_GEOMETRY_SHADER_INVOCATIONS, &max_invocations);
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO version=%s max_invocations=%d\n",
            glGetString(GL_VERSION), max_invocations);

    GLuint colour = 0, fbo = 0, vao = 0, buffer = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIDE, SIDE);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    /* Two points: left green, right blue. */
    const float points[10] = {
        -0.5f, 0.0f,  0.0f, 1.0f, 0.0f,
         0.5f, 0.0f,  0.0f, 0.0f, 1.0f,
    };
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(points), points, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 5 * sizeof(float), NULL);
    glEnableVertexAttribArray(1);
    glVertexAttribPointer(1, 3, GL_FLOAT, GL_FALSE, 5 * sizeof(float),
                          (const void *)(2 * sizeof(float)));

    /* One point, four invocations, one triangle per quadrant. */
    GLuint prog = build(gs_src);
    if (!prog) {
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(prog);
    glDrawArrays(GL_POINTS, 0, 1);
    unsigned char q[4][4] = {{0}};
    read_at(-0.5f, -0.45f, q[0]);
    read_at( 0.5f, -0.45f, q[1]);
    read_at(-0.5f,  0.55f, q[2]);
    read_at( 0.5f,  0.55f, q[3]);
    GLenum error = glGetError();
    fprintf(stderr, "INFO four_quadrants g=%u,%u,%u,%u err=0x%04x\n",
            q[0][1], q[1][1], q[2][1], q[3][1], (unsigned)error);
    /* The single point is green, so every quadrant must be green. */
    result("four_quadrants", error == GL_NO_ERROR && q[0][1] > 128 &&
                             q[1][1] > 128 && q[2][1] > 128 && q[3][1] > 128);

    /* Two points, three invocations: a 2x3 grid, left column green, right
     * blue. */
    GLuint prog2 = build(gs2_src);
    glClear(GL_COLOR_BUFFER_BIT);
    if (prog2) {
        glUseProgram(prog2);
        glDrawArrays(GL_POINTS, 0, 2);
    }
    int ok = prog2 != 0;
    for (unsigned row = 0; row < 3 && ok; ++row) {
        const float y = -0.6f + 0.6f * (float)row;
        unsigned char left[4] = {0}, right[4] = {0};
        read_at(-0.5f, y, left);
        read_at( 0.5f, y, right);
        fprintf(stderr, "INFO two_primitives row=%u left=%u,%u,%u "
                "right=%u,%u,%u\n", row, left[0], left[1], left[2],
                right[0], right[1], right[2]);
        ok = left[1] > 128 && left[2] < 64 && right[2] > 128 && right[1] < 64;
    }
    error = glGetError();
    result("two_primitives", ok && error == GL_NO_ERROR);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
