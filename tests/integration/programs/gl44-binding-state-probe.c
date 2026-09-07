/* The indexed binding state, read back.
 *
 * Three of the six remaining browser-only CTS failures are binding or state
 * queries -- shader_storage_buffer_object.basic-binding,
 * vertex_attrib_binding.basic-state4 and
 * shading_language_420pack.binding_uniform_block_array -- and all three fail
 * silently: the native reference passes, the browser fails, and no layer says
 * anything. That is the signature of a value the transport answers differently
 * rather than a call it refuses, so this asks for each value separately.
 *
 * The indexed queries looked like the shared cause: the host has to translate
 * an object name back into the guest's own numbering (that is what
 * binding_query_object is for), and the offset and size are 64-bit, which is
 * where this transport's guest/host width disagreements live -- one of them had
 * glBindVertexBuffers silently dropping every call.
 *
 * **That hypothesis was wrong.** All fourteen of these pass in the browser as
 * well as against the reference: the indexed binding state round-trips
 * correctly, names and 64-bit ranges included. The three CTS cases share a
 * word in their names and not a cause.
 *
 * It is kept because that is worth knowing. Anyone reading those three failures
 * will reach for this area first, as I did; this says it is already correct and
 * guards it against a regression while the real causes are found elsewhere.
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
    fprintf(stderr, "INFO %-40s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
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
    GLuint buf = 0; glGenBuffers(1, &buf);
    glBindBuffer(GL_ARRAY_BUFFER, buf);
    glBufferData(GL_ARRAY_BUFFER, 4096, NULL, GL_STATIC_DRAW);
    drain();

    /* Vertex binding state: the buffer name has to come back as the guest's,
       and the offset is 64-bit. */
    glBindVertexBuffer(2, buf, 256, 48);
    eq("bind_vertex_buffer_is_clean", drain(), GL_NO_ERROR);

    GLint value = 0;
    glGetIntegeri_v(0x8F4F /* GL_VERTEX_BINDING_BUFFER */, 2, &value);
    eq("vertex_binding_buffer", value, (long long)buf);
    glGetIntegeri_v(0x82D8 /* GL_VERTEX_BINDING_STRIDE */, 2, &value);
    eq("vertex_binding_stride", value, 48);
    glGetIntegeri_v(0x82D6 /* GL_VERTEX_BINDING_DIVISOR */, 2, &value);
    eq("vertex_binding_divisor", value, 0);
    GLint64 wide = 0;
    glGetInteger64i_v(0x82D7 /* GL_VERTEX_BINDING_OFFSET */, 2, &wide);
    eq("vertex_binding_offset", (long long)wide, 256);

    /* The per-attribute half of the same state. */
    glVertexAttribFormat(5, 3, GL_FLOAT, GL_FALSE, 12);
    glVertexAttribBinding(5, 2);
    drain();
    glGetVertexAttribiv(5, 0x82D4 /* GL_VERTEX_ATTRIB_BINDING */, &value);
    eq("vertex_attrib_binding", value, 2);
    glGetVertexAttribiv(5, 0x82D5 /* GL_VERTEX_ATTRIB_RELATIVE_OFFSET */,
                        &value);
    eq("vertex_attrib_relative_offset", value, 12);

    /* Shader storage bindings, where the start and size are 64-bit and the
       range has to survive the round trip intact. */
    GLuint ssbo = 0; glGenBuffers(1, &ssbo);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, ssbo);
    glBufferData(GL_SHADER_STORAGE_BUFFER, 8192, NULL, GL_STATIC_DRAW);
    glBindBufferRange(GL_SHADER_STORAGE_BUFFER, 3, ssbo, 1024, 2048);
    eq("bind_buffer_range_is_clean", drain(), GL_NO_ERROR);

    glGetIntegeri_v(0x90D3 /* GL_SHADER_STORAGE_BUFFER_BINDING */, 3, &value);
    eq("ssbo_binding", value, (long long)ssbo);
    glGetInteger64i_v(0x90D4 /* GL_SHADER_STORAGE_BUFFER_START */, 3, &wide);
    eq("ssbo_start", (long long)wide, 1024);
    glGetInteger64i_v(0x90D5 /* GL_SHADER_STORAGE_BUFFER_SIZE */, 3, &wide);
    eq("ssbo_size", (long long)wide, 2048);

    /* And the uniform-buffer equivalents, which share the code path. */
    GLuint ubo = 0; glGenBuffers(1, &ubo);
    glBindBuffer(GL_UNIFORM_BUFFER, ubo);
    glBufferData(GL_UNIFORM_BUFFER, 4096, NULL, GL_STATIC_DRAW);
    glBindBufferRange(GL_UNIFORM_BUFFER, 1, ubo, 512, 1024);
    drain();
    glGetIntegeri_v(0x8A28 /* GL_UNIFORM_BUFFER_BINDING */, 1, &value);
    eq("ubo_binding", value, (long long)ubo);
    glGetInteger64i_v(0x8A29 /* GL_UNIFORM_BUFFER_START */, 1, &wide);
    eq("ubo_start", (long long)wide, 512);
    glGetInteger64i_v(0x8A2A /* GL_UNIFORM_BUFFER_SIZE */, 1, &wide);
    eq("ubo_size", (long long)wide, 1024);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
