/* One-component 8- and 16-bit vertex attributes.
 *
 * WebGPU filled in the single-component vertex formats in its 2024 revision
 * -- unorm8, snorm8, uint8, sint8 and the 16-bit set beside them -- and the
 * driver's table never produced one, so glVertexAttribPointer(1, GL_BYTE, ...)
 * came out as "vertex element 0 format r8_sint (77) unsupported".
 *
 * That was not a wrong picture: reporting the format unsupported is what
 * hands it to u_vbuf, which converts it on the CPU before every draw. So this
 * probe is about the values being right whichever route serves them, and the
 * route itself is a question about cost -- a per-draw CPU conversion of every
 * vertex, for a format the device can now fetch directly.
 *
 * Each attribute is read back through transform feedback, which is the only
 * way to see what the vertex stage actually fetched rather than what it drew.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

static int cases, passed;
static void result(const char *name, int ok) {
    ++cases; if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 330 core\n"
    "in float a_value;\n"
    "out float v_value;\n"
    "void main() { v_value = a_value; gl_Position = vec4(0.0,0.0,0.0,1.0); }\n";
static const char *fs_src =
    "#version 330 core\n"
    "out vec4 c;\n"
    "void main() { c = vec4(1.0); }\n";

static GLuint build(void) {
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL); glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL); glCompileShader(fs);
    GLuint p = glCreateProgram();
    glAttachShader(p, vs); glAttachShader(p, fs);
    glBindAttribLocation(p, 0, "a_value");
    const char *feedback[] = { "v_value" };
    glTransformFeedbackVaryings(p, 1, feedback, GL_INTERLEAVED_ATTRIBS);
    glLinkProgram(p);
    GLint ok = 0; glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) { char log[1024] = {0}; glGetProgramInfoLog(p, sizeof log, NULL, log);
               fprintf(stderr, "INFO link failed: %s\n", log); return 0; }
    return p;
}

/* One vertex, one attribute, read back through transform feedback. */
static int fetch(GLuint prog, GLuint vbo, GLuint xfb, GLenum type,
                 GLboolean normalized, const void *bytes, size_t nbytes,
                 float *out) {
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glBufferData(GL_ARRAY_BUFFER, (GLsizeiptr)nbytes, bytes, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 1, type, normalized, 0, NULL);
    glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, xfb);
    glEnable(GL_RASTERIZER_DISCARD);
    glUseProgram(prog);
    glBeginTransformFeedback(GL_POINTS);
    glDrawArrays(GL_POINTS, 0, 1);
    glEndTransformFeedback();
    glDisable(GL_RASTERIZER_DISCARD);
    *out = 0.0f;
    glGetBufferSubData(GL_TRANSFORM_FEEDBACK_BUFFER, 0, sizeof(float), out);
    return glGetError() == GL_NO_ERROR;
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

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    GLuint prog = build();
    if (!prog) { printf("SUMMARY cases=0 passed=0 failed=0\n"); return 3; }

    GLuint vbo = 0, xfb = 0;
    glGenBuffers(1, &vbo);
    glGenBuffers(1, &xfb);
    glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, xfb);
    glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER, sizeof(float), NULL,
                 GL_DYNAMIC_READ);

    struct { const char *name; GLenum type; GLboolean norm; size_t bytes;
             unsigned char raw[4]; float want; } table[] = {
        { "byte",            GL_BYTE,           GL_FALSE, 1, {0xF9}, -7.0f },
        { "unsigned_byte",   GL_UNSIGNED_BYTE,  GL_FALSE, 1, {0xC8}, 200.0f },
        { "byte_normalized", GL_BYTE,           GL_TRUE,  1, {0x7F}, 1.0f },
        { "ubyte_normalized",GL_UNSIGNED_BYTE,  GL_TRUE,  1, {0xFF}, 1.0f },
        { "short",           GL_SHORT,          GL_FALSE, 2, {0xF8, 0xFF}, -8.0f },
        { "unsigned_short",  GL_UNSIGNED_SHORT, GL_FALSE, 2, {0x10, 0x27}, 10000.0f },
        { "short_normalized",GL_SHORT,          GL_TRUE,  2, {0xFF, 0x7F}, 1.0f },
        { "half_float",      GL_HALF_FLOAT,     GL_FALSE, 2, {0x00, 0x40}, 2.0f },
    };
    for (unsigned i = 0; i < sizeof table / sizeof table[0]; ++i) {
        float got = 0.0f;
        const int ok = fetch(prog, vbo, xfb, table[i].type, table[i].norm,
                             table[i].raw, table[i].bytes, &got);
        fprintf(stderr, "INFO %-17s got=%g want=%g ok=%d\n", table[i].name,
                (double)got, (double)table[i].want, ok);
        result(table[i].name, ok && got == table[i].want);
        while (glGetError() != GL_NO_ERROR) {}
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
