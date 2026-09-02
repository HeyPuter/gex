/* A loop whose bound is a uniform, in both GLSL dialects.
 *
 * glmark2's `loop` scenes fail to link here with
 *
 *     Tint rejected SPIR-V: error: function-scope 'var' must have a
 *     constructible type
 *
 * and their shaders are as plain as shaders get: `for (int i = 0; i < N; i++)
 * d = fract(3.0 * d);` with N a uniform, so the loop cannot be unrolled. The
 * two cases here differ only in dialect -- modern #version 330, and the
 * unversioned GLSL 1.10 with gl_FragColor that glmark2 actually writes --
 * which is what separates "loops with a dynamic bound" from "the legacy
 * path" as the thing at fault.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *modern_vertex =
    "#version 330 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";

static const char *modern_fragment =
    "#version 330 core\n"
    "uniform int Loops;\n"
    "out vec4 fragColour;\n"
    "void main() {\n"
    "    float d = fract(gl_FragCoord.x * 0.001);\n"
    "    for (int i = 0; i < Loops; i++) d = fract(3.0 * d);\n"
    "    fragColour = vec4(d, d, d, 1.0);\n"
    "}\n";

/* glmark2's loop.vert, character for character in what matters: the mat4
 * uniform and the transform through it are the part my first attempt left
 * out, and the part that turned out to matter. */
static const char *legacy_vertex =
    "attribute vec3 position;\n"
    "uniform mat4 ModelViewProjectionMatrix;\n"
    "uniform int VertexLoops;\n"
    "varying vec4 dummy;\n"
    "void main(void)\n"
    "{\n"
    "    dummy = vec4(1.0);\n"
    "    float d = fract(position.x);\n"
    "    for (int i = 0; i < VertexLoops; i++)\n"
    "        d = fract(3.0 * d);\n"
    "    vec4 pos = vec4(position.x,\n"
    "                    position.y + 0.1 * d * fract(position.x),\n"
    "                    position.z, 1.0);\n"
    "    gl_Position = ModelViewProjectionMatrix * pos;\n"
    "}\n";

static const char *legacy_fragment =
    "varying vec4 dummy;\n"
    "uniform int FragmentLoops;\n"
    "void main() {\n"
    "    vec2 FragCoord = gl_FragCoord.xy;\n"
    "    float d = fract(FragCoord.x * FragCoord.y * 0.0001);\n"
    "    for (int i = 0; i < FragmentLoops; i++) d = fract(3.0 * d);\n"
    "    gl_FragColor = vec4(d, d, d, 1.0);\n"
    "}\n";

/* Bisecting the one above: a legacy vertex shader whose only content is a
 * mat4 uniform and a transform through it. No loop, no varying. */
static const char *legacy_mat4_vertex =
    "attribute vec3 position;\n"
    "uniform mat4 ModelViewProjectionMatrix;\n"
    "void main(void) {\n"
    "    gl_Position = ModelViewProjectionMatrix * vec4(position, 1.0);\n"
    "}\n";

/* And the same without the matrix, to show which half it is. */
static const char *legacy_plain_vertex =
    "attribute vec3 position;\n"
    "void main(void) { gl_Position = vec4(position, 1.0); }\n";

static const char *legacy_plain_fragment =
    "void main(void) { gl_FragColor = vec4(1.0); }\n";

/* The pair, minimised: a mat4 uniform multiplying something a loop produced.
 * Neither half fails on its own. */
static const char *legacy_mat4_loop_vertex =
    "attribute vec3 position;\n"
    "uniform mat4 M;\n"
    "uniform int N;\n"
    "void main(void) {\n"
    "    float d = fract(position.x);\n"
    "    for (int i = 0; i < N; i++) d = fract(3.0 * d);\n"
    "    gl_Position = M * vec4(position.x, position.y + d, position.z, 1.0);\n"
    "}\n";

/* And the same shape written in modern GLSL, which is what separates "this
 * combination" from "the compatibility profile". */
static const char *modern_mat4_loop_vertex =
    "#version 330 core\n"
    "layout(location = 0) in vec3 position;\n"
    "uniform mat4 M;\n"
    "uniform int N;\n"
    "void main() {\n"
    "    float d = fract(position.x);\n"
    "    for (int i = 0; i < N; i++) d = fract(3.0 * d);\n"
    "    gl_Position = M * vec4(position.x, position.y + d, position.z, 1.0);\n"
    "}\n";

static GLuint compile(GLenum stage, const char *source, const char *what) {
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[400] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO %s compile failed: %s\n", what, log);
        return 0;
    }
    return shader;
}

static int links(const char *vs, const char *fs, const char *what) {
    GLuint vertex = compile(GL_VERTEX_SHADER, vs, what);
    GLuint fragment = compile(GL_FRAGMENT_SHADER, fs, what);
    if (!vertex || !fragment) return 0;
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[400] = {0};
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
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    /* A compatibility profile, because that is the context glmark2 gets: its
     * GLX contexts are legacy ones, and the legacy dialect below needs it.
     * Native Mesa hands out compatibility here anyway. */
    const EGLint context_attributes[] = {
        0x30FD /* EGL_CONTEXT_OPENGL_PROFILE_MASK */, 0x2 /* COMPATIBILITY */,
        EGL_NONE,
    };
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    result("modern_uniform_loop", links(modern_vertex, modern_fragment,
                                        "modern"));
    result("legacy_plain", links(legacy_plain_vertex, legacy_plain_fragment,
                                 "legacy-plain"));
    result("legacy_mat4_uniform", links(legacy_mat4_vertex,
                                        legacy_plain_fragment,
                                        "legacy-mat4"));
    result("legacy_mat4_loop", links(legacy_mat4_loop_vertex,
                                     legacy_plain_fragment, "legacy-mat4-loop"));
    result("modern_mat4_loop", links(modern_mat4_loop_vertex, modern_fragment,
                                     "modern-mat4-loop"));
    result("legacy_uniform_loop", links(legacy_vertex, legacy_fragment,
                                        "legacy"));

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
