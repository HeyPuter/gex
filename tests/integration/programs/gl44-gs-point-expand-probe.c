/* A geometry shader that takes points and emits triangles.
 *
 * Point expansion is how a billboard, a particle or an impostor is drawn: one
 * vertex in, a quad out. This driver has no geometry stage -- a geometry
 * shader runs as a compute pass that writes vertices and indices for an
 * ordinary indexed draw -- and that path used to refuse any shader whose
 * input and output primitives were not both points or both triangles.
 *
 * The refusal was hiding a real defect rather than a missing feature: the
 * count of vertices each invocation reads was derived from the *output*
 * primitive, so a points-in shader would have read three vertices per point.
 * With the two separated the case works, and this probe is what says so --
 * removing a refusal without a test only moves the failure.
 *
 *   `two_points`   two points expand into two quads, and the probe checks a
 *                  pixel inside each and one in the gap between them, so a
 *                  driver that expanded only the first point, or ran one
 *                  invocation per three points, fails
 *   `varying`      the colour is carried from the vertex through the geometry
 *                  stage, differently per point, so a shader that read the
 *                  wrong input vertex reads the wrong colour
 *
 * The quads are placed by hand in clip space rather than sized from a uniform,
 * so nothing here depends on gl_PointSize or on viewport arithmetic.
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

static GLuint build(void) {
    static const char *vs_src =
        "#version 330 core\n"
        "layout(location = 0) in vec2 centre;\n"
        "layout(location = 1) in vec3 tint;\n"
        "out vec3 vs_tint;\n"
        "void main() {\n"
        "  gl_Position = vec4(centre, 0.0, 1.0);\n"
        "  vs_tint = tint;\n"
        "}\n";
    /* One point in, a four-vertex strip out: two triangles making a quad
     * centred on the point. max_vertices = 4 with a triangle strip is the
     * smallest expansion there is. */
    static const char *gs_src =
        "#version 330 core\n"
        "layout(points) in;\n"
        "layout(triangle_strip, max_vertices = 4) out;\n"
        "in vec3 vs_tint[];\n"
        "out vec3 gs_tint;\n"
        "void main() {\n"
        "  vec4 c = gl_in[0].gl_Position;\n"
        "  float h = 0.25;\n"
        "  gs_tint = vs_tint[0];\n"
        "  gl_Position = c + vec4(-h, -h, 0.0, 0.0); EmitVertex();\n"
        "  gl_Position = c + vec4( h, -h, 0.0, 0.0); EmitVertex();\n"
        "  gl_Position = c + vec4(-h,  h, 0.0, 0.0); EmitVertex();\n"
        "  gl_Position = c + vec4( h,  h, 0.0, 0.0); EmitVertex();\n"
        "  EndPrimitive();\n"
        "}\n";
    static const char *fs_src =
        "#version 330 core\n"
        "in vec3 gs_tint;\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(gs_tint, 1.0); }\n";
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

    /* Two points, left and right of centre, far enough apart that their
     * quads (half-width 0.25) leave a gap between them. Red and blue. */
    const float points[10] = {
        -0.5f, 0.0f,  1.0f, 0.0f, 0.0f,
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

    GLuint program = build();
    if (!program) {
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(program);
    glDrawArrays(GL_POINTS, 0, 2);

    /* Clip x = -0.5 and +0.5 are pixels 16 and 48 of 64; the gap at x = 0 is
     * pixel 32, which is 0.25 away from either quad's edge. */
    unsigned char left[4] = {0}, right[4] = {0}, gap[4] = {0};
    glReadPixels(16, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, left);
    glReadPixels(48, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, right);
    glReadPixels(32, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, gap);
    const GLenum error = glGetError();
    fprintf(stderr, "INFO two_points left=%u,%u,%u right=%u,%u,%u "
            "gap=%u,%u,%u err=0x%04x\n", left[0], left[1], left[2],
            right[0], right[1], right[2], gap[0], gap[1], gap[2],
            (unsigned)error);
    /* Both quads present, and nothing between them. */
    result("two_points", error == GL_NO_ERROR &&
                         left[0] > 128 && right[2] > 128 &&
                         gap[0] < 64 && gap[1] < 64 && gap[2] < 64);
    /* The tint came through the geometry stage from the right input vertex:
     * red on the left, blue on the right, not the same colour twice. */
    fprintf(stderr, "INFO varying left_red=%u left_blue=%u right_red=%u "
            "right_blue=%u\n", left[0], left[2], right[0], right[2]);
    result("varying", error == GL_NO_ERROR &&
                      left[0] > 128 && left[2] < 64 &&
                      right[2] > 128 && right[0] < 64);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
