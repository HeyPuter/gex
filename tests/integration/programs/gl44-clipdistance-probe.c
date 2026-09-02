/* gl_ClipDistance written by the vertex stage.
 *
 * This guards the half that works, which took a driver fix to get: with
 * `compact_arrays` off, the GLSL linker packs gl_ClipDistance into a vec4[2]
 * named gl_ClipDistanceMESA and zink's SPIR-V emitter decorates *that* as
 * BuiltIn ClipDistance -- a vec4 array where SPIR-V wants a scalar one -- and
 * Tint rejects the module.
 *
 * The other half, a fragment shader *reading* gl_ClipDistance, does not work
 * and is not tested here because it cannot pass: WGSL has `clip_distances` as
 * a vertex output only, with no way to read it in a fragment shader. SPIR-V
 * does allow the fragment input, so ntv emits one and Tint rejects it with
 * `no matching call to 'bitcast<f32>(array<f32, 8>)'`. Reproduce with this
 * fragment shader against the vertex shader below, and see
 * docs/gl-webgpu-handoff.md:
 *
 *     #version 330 core
 *     in float gl_ClipDistance[gl_MaxClipDistances];
 *     out vec4 fragColour;
 *     void main() {
 *         float sum = 0.0;
 *         for (int i = 0; i < gl_MaxClipDistances; i++)
 *             sum += gl_ClipDistance[i];
 *         fragColour = vec4(sum, 1.0, 0.0, 1.0);
 *     }
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *write_vertex_source =
    "#version 330 core\n"
    "out float gl_ClipDistance[gl_MaxClipDistances];\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() {\n"
    "    for (int i = 0; i < gl_MaxClipDistances; i++)\n"
    "        gl_ClipDistance[i] = float(i + 1) / float(gl_MaxClipDistances);\n"
    "    gl_Position = vec4(pos, 0.0, 1.0);\n"
    "}\n";

static const char *plain_fragment_source =
    "#version 330 core\n"
    "out vec4 fragColour;\n"
    "void main() { fragColour = vec4(0.0, 1.0, 0.0, 1.0); }\n";

static GLuint compile(GLenum stage, const char *source, const char *what) {
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[512] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO %s compile failed: %s\n", what, log);
        return 0;
    }
    return shader;
}

static int links(const char *fragment_source, const char *what) {
    GLuint vertex = compile(GL_VERTEX_SHADER, write_vertex_source, "vertex");
    GLuint fragment = compile(GL_FRAGMENT_SHADER, fragment_source, what);
    if (!vertex || !fragment) return 0;
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glBindAttribLocation(program, 0, "pos");
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[512] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO %s link failed: %s\n", what, log);
    }
    return linked != 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
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

    GLint limit = 0;
    glGetIntegerv(GL_MAX_CLIP_DISTANCES, &limit);
    fprintf(stderr, "INFO max_clip_distances=%d err=0x%04x\n", limit,
            (unsigned)glGetError());
    /* GL requires at least 8. */
    result("clip_distance_limit", limit >= 8);

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    result("link_vertex_writes", links(plain_fragment_source, "write-only"));

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
