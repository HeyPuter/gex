/* Whether a draw stores through the image view it was actually given.
 *
 * The driver builds one texture view per image per draw -- glBindImageTexture
 * names a level and a layer the next draw may change -- and releases them as
 * soon as the bind group exists. The bind group cache keys on the view
 * *handle*, and in emdawnwebgpu a handle is a malloc'd pointer that
 * wgpuDeviceCreateBindGroup takes no reference on, so that release used to
 * free a pointer a live key still named. The next view malloc'd at the same
 * pointer then matched the key and the draw was handed its predecessor's bind
 * group -- storing into the wrong texture, silently.
 *
 * This is the shape that provokes it: several draws identical in every way
 * except which image is bound, so the only part of the key that differs is the
 * view handle. Every texture must end up written; if a handle repeats and the
 * cache matches on it, one texture is written twice and another not at all.
 *
 * Kept as a gate because the failure has no other symptom -- no validation
 * error, no refusal, and the draw itself succeeds.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

#define IMAGES 6
#define MAGIC 0xa5a5u

static int cases, passed;
static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-26s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}
static void drain(void) { while (glGetError() != GL_NO_ERROR) { } }

static GLuint build(const char *vs_src, const char *fs_src) {
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL); glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL); glCompileShader(fs);
    GLuint p = glCreateProgram();
    glAttachShader(p, vs); glAttachShader(p, fs);
    glLinkProgram(p);
    GLint ok = 0; glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[1024]; GLsizei n = 0;
        glGetProgramInfoLog(p, sizeof(log), &n, log);
        fprintf(stderr, "INFO link failed: %.*s\n", (int)n, log);
        return 0;
    }
    return p;
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
    const EGLint sa[] = { EGL_WIDTH, 64, EGL_HEIGHT, 64, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 3;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));
    drain();

    GLuint prog = build(
        "#version 430 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0); }\n",
        "#version 430 core\n"
        "layout(r32ui, binding = 0) uniform uimage2D img;\n"
        "out vec4 colour;\n"
        "void main() {\n"
        "  imageStore(img, ivec2(0, 0), uvec4(0xa5a5u, 0u, 0u, 0u));\n"
        "  colour = vec4(1.0); }\n");
    if (!prog) { printf("SUMMARY cases=0 passed=0 failed=0\n"); return 1; }

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    GLuint tex[IMAGES];
    glGenTextures(IMAGES, tex);
    for (unsigned i = 0; i < IMAGES; ++i) {
        glBindTexture(GL_TEXTURE_2D, tex[i]);
        glTexStorage2D(GL_TEXTURE_2D, 1, GL_R32UI, 1, 1);
        const GLuint zero = 0;
        glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 1, 1, GL_RED_INTEGER,
                        GL_UNSIGNED_INT, &zero);
    }
    glUseProgram(prog);
    drain();
    eq("setup_is_clean", 0, 0);

    /* Identical draws; only the bound image differs, so only the view handle
       differs in the key. A fresh view is built and released each time. */
    for (unsigned i = 0; i < IMAGES; ++i) {
        glBindImageTexture(0, tex[i], 0, GL_FALSE, 0, GL_READ_WRITE, GL_R32UI);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT |
                        GL_TEXTURE_UPDATE_BARRIER_BIT);
    }
    glFinish();
    drain();

    unsigned written = 0;
    for (unsigned i = 0; i < IMAGES; ++i) {
        GLuint back = 0;
        glBindTexture(GL_TEXTURE_2D, tex[i]);
        glGetTexImage(GL_TEXTURE_2D, 0, GL_RED_INTEGER, GL_UNSIGNED_INT,
                      &back);
        if (back == MAGIC) ++written;
        else
            fprintf(stderr, "INFO image %u holds 0x%04x, not the store\n",
                    i, back);
    }
    drain();
    /* All of them, not "most": a collision writes one twice and another never,
       so a count is the assertion and the identity of the loser is noise. */
    eq("every_image_was_written", written, IMAGES);

    printf("SUMMARY cases=%d passed=%d failed=%d\n",
           cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
